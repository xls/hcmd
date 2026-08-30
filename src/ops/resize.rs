//! `Shift+R`: resize and convert images into the other panel's directory.
//!
//! ```text
//!   holiday.png  4000x3000  ──►  50%  ──►  holiday_small.jpg  2000x1500
//! ```
//!
//! # Why it is a job and not a loop in the dialog
//!
//! A selection of two hundred photographs is one keystroke away in a file
//! manager, and decoding two hundred JPEGs takes minutes. So this is an
//! ordinary [`JobKind::Resize`](super::JobKind::Resize) on the blocking pool:
//! it reports progress per file, `Esc` stops it between files, and every
//! failure lands in the end-of-batch summary instead of stopping the batch.
//!
//! Cancellation is honoured **between files** and no finer, which is the one
//! place this differs from the copy engine. `image` decodes a whole file in
//! one call and offers no way in; the answer is not to pretend otherwise but
//! to keep the unit small, which one image is.
//!
//! # Conflicts
//!
//! The destination is the other panel's directory - the convention `F5` and
//! `F6` already follow - and a name already in the way goes through the same
//! [`Policy`] every other operation uses, so `Skip`, `Overwrite`, `Rename` and
//! their "all" variants mean here what they mean there. That is also what
//! makes writing back into the source directory possible: it is a collision
//! answered deliberately rather than a second, quieter prompt.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};

use super::conflict::{Facts, Plan, Policy};
use super::{JobContext, JobSpec};
use crate::error::{Error, Result};
use crate::vfs::{EntryKind, Vfs, VfsPath};

/// A number, and what it counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amount {
    /// A percentage of the source's own edge. `100` is the source size.
    Percent(u32),
    /// An edge length in pixels.
    Pixels(u32),
}

impl Amount {
    /// This amount against a source edge of `base`, never zero.
    ///
    /// Zero is not a picture, and a percentage that rounds to it is a typo
    /// rather than a request, so both ends clamp: one pixel at the bottom and
    /// [`MAX_EDGE`] at the top.
    pub const fn against(self, base: u32) -> u32 {
        let raw = match self {
            Self::Percent(percent) => (base as u64)
                .saturating_mul(percent as u64)
                .saturating_div(100),
            Self::Pixels(pixels) => pixels as u64,
        };
        if raw < 1 {
            1
        } else if raw > MAX_EDGE as u64 {
            MAX_EDGE
        } else {
            raw as u32
        }
    }
}

/// What a target box means when the ratio is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    /// Scale until the image fits inside the box, keeping its own proportions.
    /// One edge lands on the box and the other falls short.
    Best,
    /// Stretch to exactly the box, proportions be damned.
    Exact,
}

/// How hard the PNG encoder tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngLevel {
    /// Least CPU, largest file.
    Fast,
    /// The encoder's own balance.
    Default,
    /// Most CPU, smallest file.
    Best,
}

/// The widest or tallest an output may be.
///
/// A percentage is a multiplier and a user can type three digits, so `9999%`
/// of a 6000-pixel photograph asks for a 599,940-pixel edge and about a
/// terabyte of pixels. The cap turns that into a picture nobody wanted rather
/// than an allocation that takes the machine down with it.
pub const MAX_EDGE: u32 = 20_000;

/// The largest source this reads.
///
/// The whole file is buffered so that a member of an archive and a file on
/// this machine take the same path, and `image` needs the head of the file
/// twice anyway (once to guess the format, once to decode). 256 MiB is far
/// past any photograph and far short of a problem.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// Everything the dialog collects and the runner obeys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeSettings {
    /// Keep the source's proportions. With this on, `width` is the only
    /// dimension read and `fit` means nothing.
    pub keep_ratio: bool,
    /// The new width, and with `keep_ratio` on, the whole of the new size.
    pub width: Amount,
    /// The new height. Read only when `keep_ratio` is off.
    pub height: Amount,
    /// How the box is filled. Read only when `keep_ratio` is off.
    pub fit: FitMode,
    /// What to encode as. `None` keeps the source's own format, which is what
    /// makes a pure resize a pure resize.
    pub format: Option<ImageFormat>,
    /// JPEG quality, `1..=100`. Ignored by every other format.
    pub jpeg_quality: u8,
    /// PNG compression. Ignored by every other format.
    pub png_compression: PngLevel,
    /// Put in front of the output's name.
    pub prefix: String,
    /// Put after the output's name and before its extension.
    pub postfix: String,
}

impl Default for ResizeSettings {
    fn default() -> Self {
        Self {
            keep_ratio: true,
            width: Amount::Percent(50),
            height: Amount::Percent(50),
            fit: FitMode::Best,
            format: None,
            jpeg_quality: 85,
            png_compression: PngLevel::Default,
            prefix: String::new(),
            postfix: String::new(),
        }
    }
}

/// The size `settings` asks for, given a source of `src`.
///
/// With the ratio kept, a percentage scales both edges by itself - one
/// multiplication each, rather than a width that is then divided back into a
/// height, which rounds twice - and a pixel width has the height follow it.
pub fn target_size(src: (u32, u32), settings: &ResizeSettings) -> (u32, u32) {
    let (src_w, src_h) = (src.0.max(1), src.1.max(1));
    if settings.keep_ratio {
        return match settings.width {
            Amount::Percent(percent) => (
                Amount::Percent(percent).against(src_w),
                Amount::Percent(percent).against(src_h),
            ),
            Amount::Pixels(pixels) => {
                let width = Amount::Pixels(pixels).against(src_w);
                let height = (u64::from(src_h).saturating_mul(u64::from(width)))
                    .saturating_div(u64::from(src_w));
                (width, clamp_edge(height))
            }
        };
    }
    let box_w = settings.width.against(src_w);
    let box_h = settings.height.against(src_h);
    match settings.fit {
        FitMode::Exact => (box_w, box_h),
        // The same arithmetic `DynamicImage::resize` does, done here as well
        // so the dialog and the tests can say what the output will be without
        // decoding anything.
        FitMode::Best => {
            let by_w = f64::from(box_w) / f64::from(src_w);
            let by_h = f64::from(box_h) / f64::from(src_h);
            let scale = by_w.min(by_h);
            let width = clamp_edge((f64::from(src_w) * scale).round().max(1.0) as u64);
            let height = clamp_edge((f64::from(src_h) * scale).round().max(1.0) as u64);
            (width, height)
        }
    }
}

/// One edge, clamped into `1..=MAX_EDGE`.
const fn clamp_edge(raw: u64) -> u32 {
    if raw < 1 {
        1
    } else if raw > MAX_EDGE as u64 {
        MAX_EDGE
    } else {
        raw as u32
    }
}

/// The resampling filter for a given scale, chosen rather than asked about.
///
/// The scale that decides is the **smaller** of the two axis ratios, because
/// that axis is the one throwing information away and it is the one that will
/// alias if the kernel is too narrow.
///
/// * `scale <= 0.5` - a halving or more, so every destination pixel stands for
///   four or more source pixels. [`FilterType::Lanczos3`]'s six-tap window is
///   wide enough to see them all; anything narrower samples a fraction of the
///   pixels it is meant to average and turns fine detail into moire. The
///   ringing Lanczos adds around hard edges is the price, and at this scale it
///   is invisible next to the aliasing it prevents.
/// * `0.5 < scale < 1.0` - a moderate downscale, where two to four source
///   pixels meet in one destination pixel. [`FilterType::CatmullRom`]'s
///   four-tap cubic already covers that neighbourhood, and it rings visibly
///   less on the text and line art that a moderate shrink is usually of.
/// * `scale >= 1.0` - an upscale, or no change at all. There is no aliasing to
///   fight: no source detail is being discarded, so Lanczos would contribute
///   only its ringing, drawn at the larger size where it shows. CatmullRom
///   again.
///
/// No filter picker in the dialog, deliberately. The right answer is a
/// function of the numbers the user already typed, and a control that can only
/// be set wrong is not a feature.
pub fn filter_for(src: (u32, u32), dst: (u32, u32)) -> FilterType {
    let by_w = f64::from(dst.0) / f64::from(src.0.max(1));
    let by_h = f64::from(dst.1) / f64::from(src.1.max(1));
    if by_w.min(by_h) <= 0.5 {
        FilterType::Lanczos3
    } else {
        FilterType::CatmullRom
    }
}

/// `image` resampled to `dst`, or the image itself when `dst` is its own size.
///
/// The identity case is the convert-only run - "save these forty PNGs as
/// JPEG" - and it is worth its own branch: resampling an image to the size it
/// already is costs the same as resampling it to any other and puts a filter
/// through pixels that did not need one.
pub fn scaled(image: &DynamicImage, dst: (u32, u32)) -> DynamicImage {
    let src = (image.width(), image.height());
    if src == dst {
        return image.clone();
    }
    image.resize_exact(dst.0, dst.1, filter_for(src, dst))
}

/// The name an output takes, from the source's own name.
///
/// The extension comes from the **format**, never from the source: a PNG saved
/// as JPEG that kept its `.png` is a file that lies about itself to every
/// other program on the machine.
pub fn output_name(source_name: &str, format: ImageFormat, settings: &ResizeSettings) -> String {
    let stem = Path::new(source_name).file_stem().map_or_else(
        || source_name.to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let ext = format.extensions_str().first().copied().unwrap_or("img");
    format!("{}{stem}{}.{ext}", settings.prefix, settings.postfix)
}

/// The output's name when the source's format may not be known.
///
/// The dialog's preview line and the job's own naming have to agree, so they
/// are one function with one fallback: with a format in hand this *is*
/// [`output_name`], and without one - nothing recognised the header, and the
/// user left the format on `same as source` - the source's own extension is
/// kept, which is what "same as source" will turn out to mean.
pub fn preview_name(
    source_name: &str,
    format: Option<ImageFormat>,
    settings: &ResizeSettings,
) -> String {
    if let Some(format) = format {
        return output_name(source_name, format, settings);
    }
    let path = Path::new(source_name);
    let stem = path.file_stem().map_or_else(
        || source_name.to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    match ext {
        Some(ext) => format!("{}{stem}{}.{ext}", settings.prefix, settings.postfix),
        None => format!("{}{stem}{}", settings.prefix, settings.postfix),
    }
}

/// Encode `image` into memory.
///
/// In memory and not straight to the file because the whole point of the
/// temporary-and-rename below is that a half-written picture never appears
/// under the final name, and an encoder that fails half way through has
/// already written half a file.
///
/// # The colour model is the source's
///
/// Every branch hands the [`DynamicImage`] to `write_with_encoder`, which
/// writes the colour type the image actually has and converts **only** where
/// the encoder cannot take it. That is the whole of this section, and it is
/// load-bearing: an earlier version flattened everything to `to_rgba8()`
/// first, which gave every opaque PNG a fully opaque alpha plane it did not
/// have before - four bytes per pixel where three would do, carrying no
/// information, and measurably larger files. An image with no alpha comes out
/// with no alpha, and greyscale stays greyscale.
///
/// The one conversion that is real rather than accidental is JPEG's: the
/// format has no alpha channel at all, so an RGBA source targeting JPEG loses
/// it (and a greyscale one stays greyscale, which JPEG can carry).
///
/// # Why a resized PNG can still be larger
///
/// Not a defect and not fixable here: resampling a screenshot or a diagram
/// invents intermediate colours where there were hard edges, and PNG's filters
/// and deflate live on runs of identical pixels. The smoothing that makes the
/// smaller picture look right is the same thing that makes it compress worse,
/// so a PNG of flat colour or text may grow. Every image tool does this; a
/// quantiser to chase it back would be a different feature with its own
/// losses.
pub fn encode(
    image: &DynamicImage,
    format: ImageFormat,
    settings: &ResizeSettings,
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    match format {
        // `image` 0.25 decodes WebP and does not encode it, so this is the one
        // format that is a source and not a target. Refused by name, before
        // anything is written: the alternative is the encoder's own
        // "unsupported" a moment later, which reads as a bug in this program.
        ImageFormat::WebP => {
            return Err(Error::msg(
                "webp cannot be written by this build - choose PNG or JPEG as the output format",
            ));
        }
        ImageFormat::Jpeg => {
            let quality = settings.jpeg_quality.clamp(1, 100);
            image
                .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut out, quality,
                ))
                .map_err(|e| Error::msg(e.to_string()))?;
        }
        ImageFormat::Png => {
            let compression = match settings.png_compression {
                PngLevel::Fast => image::codecs::png::CompressionType::Fast,
                PngLevel::Default => image::codecs::png::CompressionType::Default,
                PngLevel::Best => image::codecs::png::CompressionType::Best,
            };
            image
                .write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
                    &mut out,
                    compression,
                    image::codecs::png::FilterType::Adaptive,
                ))
                .map_err(|e| Error::msg(e.to_string()))?;
        }
        other => {
            let mut cursor = std::io::Cursor::new(&mut out);
            image
                .write_to(&mut cursor, other)
                .map_err(|e| Error::msg(e.to_string()))?;
        }
    }
    Ok(out)
}

/// What became of one source.
enum Done {
    /// Written, and this many source bytes were read to do it.
    Wrote(u64),
    /// A conflict was answered `Skip`.
    Skipped,
    /// The user cancelled from the conflict dialog; the batch stops.
    Stopped,
}

/// The `Shift+R` runner.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    let Some(settings) = spec.options.resize.clone() else {
        ctx.start(0, 0);
        for source in &spec.sources {
            ctx.fail(source, "no resize settings were given");
        }
        return;
    };
    let Some(dest) = spec.dest.as_ref() else {
        ctx.start(0, 0);
        for source in &spec.sources {
            ctx.fail(source, "no destination was given");
        }
        return;
    };
    let Some(dest_dir) = dest.local_path().map(Path::to_path_buf) else {
        // Refused up front rather than once per file: an encoder writes a
        // whole file at a time and there is nothing to be gained by
        // discovering that two hundred times.
        ctx.start(0, 0);
        ctx.fail(dest, "images are written onto a real filesystem");
        return;
    };

    let mut bytes_total = 0_u64;
    for source in &spec.sources {
        if let Ok(entry) = vfs.stat(source) {
            bytes_total = bytes_total.saturating_add(entry.size);
        }
    }
    ctx.start(
        u64::try_from(spec.sources.len()).unwrap_or(u64::MAX),
        bytes_total,
    );

    let mut policy = Policy::new(spec.options.conflict);
    for source in &spec.sources {
        if ctx.cancelled() {
            break;
        }
        match one(vfs, source, &dest_dir, &settings, &mut policy, ctx) {
            Ok(Done::Wrote(read)) => {
                ctx.add_bytes(read);
                ctx.add_file();
            }
            Ok(Done::Skipped) => ctx.add_skipped(),
            Ok(Done::Stopped) => break,
            Err(err) => {
                ctx.fail(source, err);
                if ctx.fatal() {
                    break;
                }
            }
        }
    }
}

/// Resize one source into `dest_dir`.
fn one(
    vfs: &dyn Vfs,
    source: &VfsPath,
    dest_dir: &Path,
    settings: &ResizeSettings,
    policy: &mut Policy,
    ctx: &mut JobContext,
) -> Result<Done> {
    let entry = vfs.stat(source)?;
    if entry.kind == EntryKind::Dir {
        return Err(Error::msg("a directory is not an image"));
    }
    if entry.size > MAX_SOURCE_BYTES {
        return Err(Error::msg(format!(
            "{} bytes is larger than this reads ({MAX_SOURCE_BYTES})",
            entry.size
        )));
    }
    ctx.set_file(&source.to_string(), entry.size);

    // `take` and not the reported size: a size from an archive index or a
    // remote listing is a claim, and this is the one place a lie about it
    // would become an allocation.
    let mut raw: Vec<u8> = Vec::new();
    vfs.open_read(source)?
        .take(MAX_SOURCE_BYTES.saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(Error::Bare)?;
    let read = u64::try_from(raw.len()).unwrap_or(0);
    if read > MAX_SOURCE_BYTES {
        return Err(Error::msg("larger than this reads"));
    }

    let reader = ImageReader::new(std::io::Cursor::new(&raw))
        .with_guessed_format()
        .map_err(Error::Bare)?;
    // The format the bytes are in, not the format the name claims: a `.jpg`
    // that is really a PNG is a file this program already opens correctly
    // everywhere else.
    let sniffed = reader.format();
    let image = reader.decode().map_err(|e| Error::msg(e.to_string()))?;
    let format = match settings.format.or(sniffed) {
        Some(format) => format,
        None => return Err(Error::msg("nothing here says what image format this is")),
    };

    let target = target_size((image.width(), image.height()), settings);
    let encoded = encode(&scaled(&image, target), format, settings)?;

    let name = output_name(&entry.name, format, settings);
    let dst = dest_dir.join(&name);
    let dst_vfs = VfsPath::local(&dst);
    let facts = Facts {
        is_dir: false,
        size: entry.size,
        mtime: entry.mtime,
    };
    let shown = source
        .local_path()
        .unwrap_or_else(|| Path::new(&entry.name));
    match policy.resolve_from(shown, facts, &dst, &dst_vfs, ctx) {
        Plan::Write(at) | Plan::Replace(at) => {
            commit(&at, &encoded)?;
            Ok(Done::Wrote(read))
        }
        // There is no such thing as half an image appended to another one, and
        // the conflict dialog only offers `Append` where it means something.
        Plan::Append(_) => Err(Error::msg("an image cannot be appended to another")),
        Plan::Skip => Ok(Done::Skipped),
        Plan::Refuse(why) => Err(Error::msg(why)),
        Plan::Stop => Ok(Done::Stopped),
    }
}

/// Write `bytes` beside `at` and rename them onto it.
///
/// The rename is what makes the destination either the old file or the whole
/// new one and never a truncated picture, and the `sync_all` is what makes the
/// bytes durable before the rename claims they are - the same rule
/// `ops::copy`'s commit follows, and for the same reason.
fn commit(at: &Path, bytes: &[u8]) -> Result<u64> {
    let Some(name) = at.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Err(Error::InvalidPath(format!(
            "{}: no file name to write",
            at.display()
        )));
    };
    let tmp: PathBuf = at.with_file_name(format!(".{name}.hcmd-resize-{}", std::process::id()));
    let mut file = fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
    let outcome = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| Error::io(&tmp, e));
    drop(file);
    if let Err(err) = outcome {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, at) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::io(at, err));
    }
    Ok(u64::try_from(bytes.len()).unwrap_or(0))
}

#[cfg(test)]
#[path = "resize_tests.rs"]
mod tests;
