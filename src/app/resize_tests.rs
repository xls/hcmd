//! Tests for [`super`], against real image headers built here.

use super::*;
use image::{ImageFormat, Rgb, RgbImage};

/// A request for one file of `size` bytes called `name`.
fn request(name: &str, size: u64, count: usize) -> ResizeRequest {
    ResizeRequest {
        path: VfsPath::local(format!("/tmp/{name}")),
        name: name.to_string(),
        size,
        count,
        destination: "/photos/small".to_string(),
    }
}

/// The bytes of a `w` by `h` image in `format`.
fn encoded(w: u32, h: u32, format: ImageFormat) -> Vec<u8> {
    let mut image = RgbImage::new(w, h);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut out, format)
        .expect("encode the fixture");
    out.into_inner()
}

#[test]
fn one_png_is_described_by_its_own_header() {
    let head = encoded(640, 480, ImageFormat::Png);
    let subject = describe(&request("holiday.png", head.len() as u64, 1), &head);
    assert_eq!(subject.dimensions, Some((640, 480)));
    assert_eq!(subject.format, Some(ImageFormat::Png));
    assert_eq!(subject.count, 1);
    assert_eq!(subject.name, "holiday.png");
}

#[test]
fn a_jpeg_is_measured_too_which_the_templates_cannot_do() {
    // The reason this reads through `image` rather than through the file
    // templates: a JPEG's size is in its SOF marker, and
    // `templates/image/jpeg.toml` has no Dimensions line to give.
    let head = encoded(321, 123, ImageFormat::Jpeg);
    let subject = describe(&request("photo.jpg", head.len() as u64, 1), &head);
    assert_eq!(subject.dimensions, Some((321, 123)));
    assert_eq!(subject.format, Some(ImageFormat::Jpeg));
}

#[test]
fn a_header_that_cannot_be_read_still_describes_the_selection() {
    // The dialog opens either way: a file this program cannot measure is
    // still one the user may be asking it to convert.
    let subject = describe(&request("notes.txt", 12, 1), b"this is not a picture");
    assert_eq!(subject.dimensions, None);
    assert_eq!(subject.format, None);
    assert_eq!(subject.name, "notes.txt");
    assert_eq!(subject.size, 12);
}

#[test]
fn an_empty_head_is_not_an_error_either() {
    let subject = describe(&request("gone.png", 0, 1), &[]);
    assert_eq!(subject.dimensions, None);
    assert_eq!(subject.format, None);
}

#[test]
fn a_truncated_header_is_the_unreadable_case_and_not_a_wrong_answer() {
    // Half a PNG signature names no format and measures nothing, rather than
    // guessing from the four bytes that did arrive.
    let head = encoded(64, 64, ImageFormat::Png);
    let cut = head.get(..4).unwrap_or_default();
    let subject = describe(&request("half.png", head.len() as u64, 1), cut);
    assert_eq!(subject.dimensions, None);
}

#[test]
fn a_count_travels_with_the_first_image_for_the_preview() {
    // With several selected the header says how many; the first one is still
    // described, because the name preview is about it.
    let head = encoded(100, 50, ImageFormat::Png);
    let subject = describe(&request("a.png", head.len() as u64, 12), &head);
    assert_eq!(subject.count, 12);
    assert_eq!(subject.dimensions, Some((100, 50)));
}

#[test]
fn the_request_slot_holds_one_and_the_newest_wins() {
    // A second Shift+R replaces the first: the older answer would open a
    // dialog about a selection that has already changed.
    let mut app = App::headless(
        crate::config::Config::default(),
        crate::config::Keymap::builtin(),
        crate::config::Theme::blue(),
    );
    app.request_resize(request("first.png", 1, 1));
    app.request_resize(request("second.png", 2, 3));
    let taken = app.take_pending_resize().expect("a request was queued");
    assert_eq!(taken.name, "second.png");
    assert_eq!(taken.count, 3);
    assert!(app.take_pending_resize().is_none(), "and only one");
}
