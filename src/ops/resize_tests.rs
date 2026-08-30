//! Tests for [`super`], against real images generated here.
//!
//! Every assertion about an output decodes it again and reads the dimensions
//! and the format back off the bytes. Trusting the call that wrote the file to
//! say what it wrote is how a resizer that silently ignores its own settings
//! passes its own suite.

use super::*;
use crate::ops::{JobContext, JobKind, JobOptions, JobSpec};
use crate::vfs::LocalFs;
use image::{ImageFormat, Rgba, RgbaImage};

/// A directory that removes itself, under the system temp directory.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let at = std::env::temp_dir().join(format!(
            "hcmd-resize-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&at).expect("temp dir");
        Self(at)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A `w` by `h` PNG with a pattern in it, written to `at`.
///
/// A pattern and not a flat colour: a flat image compresses to the same bytes
/// at every JPEG quality, which would make the quality test pass on an
/// encoder that ignored the setting.
fn write_png(at: &Path, w: u32, h: u32) {
    let mut image = RgbaImage::new(w, h);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let noise = ((x * 7 + y * 13) % 251) as u8;
        *pixel = Rgba([noise, x as u8, y as u8, 255]);
    }
    DynamicImage::ImageRgba8(image)
        .save_with_format(at, ImageFormat::Png)
        .expect("write the source png");
}

/// The dimensions and format of the file at `at`, read off its bytes.
fn decoded(at: &Path) -> ((u32, u32), ImageFormat) {
    let reader = ImageReader::open(at)
        .expect("open the output")
        .with_guessed_format()
        .expect("guess the output format");
    let format = reader.format().expect("the output names a format");
    let image = reader.decode().expect("decode the output");
    ((image.width(), image.height()), format)
}

/// Run a resize job to completion, synchronously, and hand back its summary.
fn drive(sources: Vec<PathBuf>, dest: &Path, settings: ResizeSettings) -> crate::ops::JobSummary {
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::Resize);
    let spec = JobSpec::new(
        JobKind::Resize,
        sources.iter().map(VfsPath::local).collect(),
        Some(VfsPath::local(dest)),
    )
    .with_options(JobOptions {
        resize: Some(settings),
        ..JobOptions::default()
    });
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    ctx.finish()
}

#[test]
fn a_percentage_scales_both_edges_and_the_output_says_so() {
    let dir = Temp::new("percent");
    let src = dir.join("a.png");
    write_png(&src, 400, 300);
    let out = Temp::new("percent-out");

    let summary = drive(
        vec![src],
        &out.0,
        ResizeSettings {
            keep_ratio: true,
            width: Amount::Percent(50),
            ..ResizeSettings::default()
        },
    );
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 1);

    let (size, format) = decoded(&out.join("a.png"));
    assert_eq!(size, (200, 150), "half of 400x300");
    assert_eq!(format, ImageFormat::Png, "the format was kept");
}

#[test]
fn a_pixel_width_with_the_ratio_kept_brings_the_height_with_it() {
    let dir = Temp::new("pixels");
    let src = dir.join("b.png");
    write_png(&src, 400, 300);
    let out = Temp::new("pixels-out");

    let summary = drive(
        vec![src],
        &out.0,
        ResizeSettings {
            keep_ratio: true,
            width: Amount::Pixels(100),
            ..ResizeSettings::default()
        },
    );
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(decoded(&out.join("b.png")).0, (100, 75));
}

#[test]
fn without_the_ratio_an_exact_fit_stretches_and_a_best_fit_does_not() {
    let dir = Temp::new("fit");
    let src = dir.join("c.png");
    write_png(&src, 400, 300);

    let exact = Temp::new("fit-exact");
    let summary = drive(
        vec![src.clone()],
        &exact.0,
        ResizeSettings {
            keep_ratio: false,
            width: Amount::Pixels(200),
            height: Amount::Pixels(200),
            fit: FitMode::Exact,
            ..ResizeSettings::default()
        },
    );
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(
        decoded(&exact.join("c.png")).0,
        (200, 200),
        "exact means exact, proportions be damned"
    );

    let best = Temp::new("fit-best");
    let summary = drive(
        vec![src],
        &best.0,
        ResizeSettings {
            keep_ratio: false,
            width: Amount::Pixels(200),
            height: Amount::Pixels(200),
            fit: FitMode::Best,
            ..ResizeSettings::default()
        },
    );
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(
        decoded(&best.join("c.png")).0,
        (200, 150),
        "the 4:3 source fits inside a 200x200 box at 200x150"
    );
}

#[test]
fn a_convert_only_run_changes_the_format_and_leaves_the_size_alone() {
    // "save these forty PNGs as JPEG": the common case, and the one where a
    // resizer that always resamples would spend its time for nothing.
    let dir = Temp::new("convert");
    let src = dir.join("d.png");
    write_png(&src, 320, 240);
    let out = Temp::new("convert-out");

    let summary = drive(
        vec![src],
        &out.0,
        ResizeSettings {
            keep_ratio: true,
            width: Amount::Percent(100),
            format: Some(ImageFormat::Jpeg),
            ..ResizeSettings::default()
        },
    );
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);

    let (size, format) = decoded(&out.join("d.jpg"));
    assert_eq!(size, (320, 240), "the dimensions were left alone");
    assert_eq!(format, ImageFormat::Jpeg);
    assert!(
        !out.join("d.png").exists(),
        "the extension follows the format, not the source"
    );
}

#[test]
fn the_jpeg_quality_setting_reaches_the_encoder() {
    let dir = Temp::new("quality");
    let src = dir.join("e.png");
    write_png(&src, 200, 200);

    let mut sizes = Vec::new();
    for quality in [20_u8, 95] {
        let out = Temp::new("quality-out");
        let summary = drive(
            vec![src.clone()],
            &out.0,
            ResizeSettings {
                keep_ratio: true,
                width: Amount::Percent(100),
                format: Some(ImageFormat::Jpeg),
                jpeg_quality: quality,
                ..ResizeSettings::default()
            },
        );
        assert!(summary.failures.is_empty(), "{:?}", summary.failures);
        let at = out.join("e.jpg");
        assert_eq!(decoded(&at).1, ImageFormat::Jpeg);
        sizes.push(fs::metadata(&at).expect("the output exists").len());
    }
    assert_eq!(sizes.len(), 2, "both qualities were encoded");
    let (low, high) = (sizes[0], sizes[1]);
    assert!(
        low < high,
        "quality 20 produced {low} bytes and quality 95 produced {high}; \
         the setting is being ignored"
    );
}

#[test]
fn the_png_compression_setting_reaches_the_encoder() {
    let dir = Temp::new("deflate");
    let src = dir.join("f.png");
    write_png(&src, 300, 300);

    let mut sizes = Vec::new();
    for level in [PngLevel::Fast, PngLevel::Best] {
        let out = Temp::new("deflate-out");
        let summary = drive(
            vec![src.clone()],
            &out.0,
            ResizeSettings {
                keep_ratio: true,
                width: Amount::Percent(100),
                format: Some(ImageFormat::Png),
                png_compression: level,
                ..ResizeSettings::default()
            },
        );
        assert!(summary.failures.is_empty(), "{:?}", summary.failures);
        sizes.push(
            fs::metadata(out.join("f.png"))
                .expect("the output exists")
                .len(),
        );
    }
    assert_eq!(sizes.len(), 2);
    assert!(
        sizes[1] <= sizes[0],
        "Best produced {} bytes against Fast's {}",
        sizes[1],
        sizes[0]
    );
}

#[test]
fn webp_is_refused_as_a_target_rather_than_written_broken() {
    let dir = Temp::new("webp");
    let src = dir.join("g.png");
    write_png(&src, 64, 64);
    let out = Temp::new("webp-out");

    let summary = drive(
        vec![src],
        &out.0,
        ResizeSettings {
            format: Some(ImageFormat::WebP),
            ..ResizeSettings::default()
        },
    );
    assert_eq!(summary.files_done, 0, "nothing was written");
    assert_eq!(summary.failures.len(), 1, "one refusal, naming the file");
    let reason = &summary.failures[0].error;
    assert!(
        reason.contains("webp"),
        "the refusal says which format: {reason}"
    );
    assert!(
        !out.join("g.webp").exists(),
        "and no half-made file was left behind"
    );
}

#[test]
fn the_prefix_and_the_postfix_land_around_the_stem() {
    let settings = ResizeSettings {
        prefix: "small_".to_string(),
        postfix: "-web".to_string(),
        ..ResizeSettings::default()
    };
    assert_eq!(
        output_name("holiday.png", ImageFormat::Jpeg, &settings),
        "small_holiday-web.jpg"
    );
    assert_eq!(
        output_name("holiday.png", ImageFormat::Png, &ResizeSettings::default()),
        "holiday.png",
        "with neither set the name is the source's own"
    );
}

#[test]
fn the_filter_follows_the_scale_and_nothing_else() {
    // The thresholds the doc comment states, asserted so a later edit to one
    // of them has to change this test as well.
    assert_eq!(
        filter_for((1000, 1000), (500, 500)),
        FilterType::Lanczos3,
        "a halving is a heavy downscale"
    );
    assert_eq!(filter_for((1000, 1000), (100, 100)), FilterType::Lanczos3);
    assert_eq!(
        filter_for((1000, 1000), (600, 600)),
        FilterType::CatmullRom,
        "a moderate downscale rings less with a four-tap cubic"
    );
    assert_eq!(
        filter_for((1000, 1000), (1000, 1000)),
        FilterType::CatmullRom
    );
    assert_eq!(
        filter_for((1000, 1000), (2000, 2000)),
        FilterType::CatmullRom,
        "an upscale has no aliasing to fight"
    );
    assert_eq!(
        filter_for((1000, 1000), (900, 100)),
        FilterType::Lanczos3,
        "the axis that shrinks most is the one that decides"
    );
}

#[test]
fn an_edge_is_never_zero_and_never_unbounded() {
    // A percentage is a multiplier and the field takes three digits.
    assert_eq!(
        Amount::Percent(1).against(10),
        1,
        "0.1 of 10 is not nothing"
    );
    assert_eq!(Amount::Percent(0).against(4000), 1);
    assert_eq!(Amount::Percent(9999).against(6000), MAX_EDGE);
    assert_eq!(Amount::Pixels(0).against(100), 1);
    assert_eq!(Amount::Pixels(u32::MAX).against(100), MAX_EDGE);
}

#[test]
fn a_directory_fails_the_one_source_and_not_the_batch() {
    let dir = Temp::new("mixed");
    let src = dir.join("h.png");
    write_png(&src, 40, 40);
    let inner = dir.join("subdir");
    fs::create_dir_all(&inner).expect("a directory among the marks");
    let out = Temp::new("mixed-out");

    let summary = drive(
        vec![inner, src],
        &out.0,
        ResizeSettings {
            width: Amount::Percent(50),
            ..ResizeSettings::default()
        },
    );
    assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    assert_eq!(summary.files_done, 1, "the image after it still ran");
    assert_eq!(decoded(&out.join("h.png")).0, (20, 20));
}

#[test]
fn a_file_that_is_not_an_image_is_a_failure_with_a_reason() {
    let dir = Temp::new("garbage");
    let src = dir.join("notes.txt");
    fs::write(&src, b"this is not a picture").expect("write the decoy");
    let out = Temp::new("garbage-out");

    let summary = drive(vec![src], &out.0, ResizeSettings::default());
    assert_eq!(summary.files_done, 0);
    assert_eq!(summary.failures.len(), 1);
    assert!(
        !summary.failures[0].error.is_empty(),
        "the summary says why"
    );
}

#[test]
fn an_existing_destination_is_skipped_when_the_policy_says_so() {
    // The conflict machinery is the one every other operation uses, so this
    // asserts the wiring rather than the policy: a standing `Skip` must leave
    // the file that is already there untouched.
    let dir = Temp::new("conflict");
    let src = dir.join("i.png");
    write_png(&src, 100, 100);
    let out = Temp::new("conflict-out");
    let existing = out.join("i.png");
    fs::write(&existing, b"not an image, and it must survive").expect("seed the collision");

    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::Resize);
    let spec = JobSpec::new(
        JobKind::Resize,
        vec![VfsPath::local(&src)],
        Some(VfsPath::local(&out.0)),
    )
    .with_options(JobOptions {
        resize: Some(ResizeSettings::default()),
        conflict: Some(crate::ops::ConflictChoice::Skip),
        ..JobOptions::default()
    });
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();

    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.files_done, 0);
    assert_eq!(
        fs::read(&existing).expect("still there"),
        b"not an image, and it must survive"
    );
}

#[test]
fn a_cancelled_batch_stops_between_files() {
    let dir = Temp::new("cancel");
    let mut sources = Vec::new();
    for n in 0..4 {
        let at = dir.join(&format!("j{n}.png"));
        write_png(&at, 60, 60);
        sources.push(VfsPath::local(&at));
    }
    let out = Temp::new("cancel-out");

    let (mut ctx, _rx, _decisions, cancel) = JobContext::for_test(JobKind::Resize);
    let spec = JobSpec::new(JobKind::Resize, sources, Some(VfsPath::local(&out.0))).with_options(
        JobOptions {
            resize: Some(ResizeSettings::default()),
            ..JobOptions::default()
        },
    );
    cancel.cancel();
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();
    assert!(summary.cancelled);
    assert_eq!(summary.files_done, 0);
}

#[test]
fn the_destination_must_be_a_real_filesystem() {
    let dir = Temp::new("nonlocal");
    let src = dir.join("k.png");
    write_png(&src, 32, 32);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::Resize);
    let inside_archive =
        VfsPath::local(dir.join("bundle.zip")).with_segment(crate::vfs::BackendKind::Archive, "/");
    let spec = JobSpec::new(
        JobKind::Resize,
        vec![VfsPath::local(&src)],
        Some(inside_archive),
    )
    .with_options(JobOptions {
        resize: Some(ResizeSettings::default()),
        ..JobOptions::default()
    });
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();
    assert_eq!(summary.failures.len(), 1, "refused once, up front");
    assert_eq!(summary.files_done, 0);
}
