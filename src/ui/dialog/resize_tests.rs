//! Tests for [`super`].

use super::*;
use crate::app::resize::ResizeSubject;
use crate::input::{KeyModifiers, KeyPress};

/// One 4000x3000 PNG called `holiday.png`, which is what most of these are
/// about: the dialog's own behaviour does not depend on the numbers.
fn one_image() -> ResizeSubject {
    ResizeSubject {
        name: "holiday.png".to_string(),
        size: 11_927_552,
        count: 1,
        format: Some(image::ImageFormat::Png),
        dimensions: Some((4000, 3000)),
        destination: "/photos/small".to_string(),
    }
}

/// The same selection, but `count` images in it.
fn many(count: usize) -> ResizeSubject {
    ResizeSubject {
        count,
        ..one_image()
    }
}

fn key(code: KeyCode) -> DialogKey {
    DialogKey::raw(KeyPress::plain(code))
}

fn typed(c: char) -> DialogKey {
    DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::NONE))
}

/// `Alt`+letter.
fn alt(c: char) -> DialogKey {
    DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
}

/// Every character drawn underlined, folded to lower case.
///
/// Read off the rendered buffer rather than off the table, so a declared
/// letter with no paint behind it fails the test that uses this.
fn underlined(d: &ResizeDialog, w: u16, h: u16) -> Vec<char> {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let style = DialogStyle::new(
        &crate::config::Theme::blue(),
        crate::config::ColorDepth::TrueColor,
        false,
    );
    terminal
        .draw(|f| {
            crate::dialog::draw(f, d, f.area(), &style);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let Some(cell) = buffer.cell((x, y)) else {
                continue;
            };
            if cell.modifier.contains(ratatui::style::Modifier::UNDERLINED) {
                out.extend(cell.symbol().chars());
            }
        }
    }
    out.iter().map(|c| c.to_ascii_lowercase()).collect()
}

/// The dialog's own interior, as text, one string per row.
fn rendered(d: &ResizeDialog, w: u16, h: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let style = DialogStyle::new(
        &crate::config::Theme::blue(),
        crate::config::ColorDepth::TrueColor,
        false,
    );
    terminal
        .draw(|f| {
            let area = f.area();
            d.render(f, area, &style);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .filter_map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
                .collect::<String>()
        })
        .collect()
}

#[test]
fn it_opens_on_a_kept_ratio_and_a_percentage() {
    let d = ResizeDialog::new(one_image());
    let settings = d.settings();
    assert!(settings.keep_ratio);
    assert_eq!(settings.width, Amount::Percent(50));
    assert_eq!(settings.format, None, "same as source, until it is changed");
    assert_eq!(d.focused(), Control::KeepRatio);
}

#[test]
fn a_number_typed_into_the_width_reaches_the_settings() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Width);
    for _ in 0..4 {
        d.handle_key(&key(KeyCode::Backspace));
    }
    for c in "800".chars() {
        d.handle_key(&typed(c));
    }
    assert_eq!(d.settings().width, Amount::Percent(800));

    // And the unit stepper turns the same number into pixels.
    d.focus_control(Control::WidthUnit);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.settings().width, Amount::Pixels(800));
}

#[test]
fn a_letter_never_lands_in_a_size_field() {
    // A size is a number, and the key is swallowed rather than passed on: a
    // dialog consumes all input.
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Width);
    for c in "abc".chars() {
        assert!(matches!(d.handle_key(&typed(c)), DialogOutcome::Consumed));
    }
    assert_eq!(d.settings().width, Amount::Percent(50));
}

#[test]
fn space_toggles_keep_ratio_and_alt_k_only_ever_turns_it_on() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::KeepRatio);
    d.handle_key(&typed(' '));
    assert!(!d.settings().keep_ratio);
    d.handle_key(&alt('k'));
    assert!(d.settings().keep_ratio, "Alt+K ticked it");
    d.handle_key(&alt('k'));
    assert!(d.settings().keep_ratio, "and again left it ticked");
    d.handle_key(&typed(' '));
    assert!(!d.settings().keep_ratio, "Space is the toggle");
}

#[test]
fn the_fit_stepper_is_inert_while_the_ratio_is_kept() {
    // Greyed and still in the `Tab` order, and refusing in place rather than
    // silently doing nothing: the user pressed a key at this control.
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Fit);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.settings().fit, FitMode::Best, "nothing was stepped");
    assert!(d.error().is_some(), "and it said why");
    assert_eq!(d.focused(), Control::Fit, "the focus did not move on");

    d.focus_control(Control::KeepRatio);
    d.handle_key(&typed(' '));
    d.focus_control(Control::Fit);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(
        d.settings().fit,
        FitMode::Exact,
        "with the ratio off it steps"
    );
}

#[test]
fn the_format_stepper_walks_the_offered_list_and_wraps() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Format);
    assert_eq!(d.format(), None);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.format(), Some(image::ImageFormat::Png));
    d.handle_key(&key(KeyCode::Left));
    assert_eq!(d.format(), None, "and back");
    d.handle_key(&key(KeyCode::Left));
    assert_eq!(
        d.format(),
        FORMATS.last().copied().flatten(),
        "stepping back off the front wraps to the end"
    );
}

#[test]
fn webp_is_not_on_the_list_because_it_cannot_be_written() {
    // Offering a choice that can only fail at `OK` is the opposite of refusing
    // up front.
    assert!(!FORMATS.contains(&Some(image::ImageFormat::WebP)));
    assert_eq!(FORMATS.first().copied(), Some(None), "same as source first");
}

#[test]
fn the_quality_stepper_follows_the_format() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Format);
    d.handle_key(&key(KeyCode::Right));
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.format(), Some(image::ImageFormat::Jpeg));
    d.focus_control(Control::Quality);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.settings().jpeg_quality, 90);
    for _ in 0..40 {
        d.handle_key(&key(KeyCode::Right));
    }
    assert_eq!(
        d.settings().jpeg_quality,
        100,
        "it clamps rather than wraps"
    );

    d.focus_control(Control::Format);
    d.handle_key(&key(KeyCode::Left));
    assert_eq!(d.format(), Some(image::ImageFormat::Png));
    d.focus_control(Control::Quality);
    d.handle_key(&key(KeyCode::Right));
    assert_eq!(d.settings().png_compression, PngLevel::Best);
    assert!(d.has_quality());

    d.focus_control(Control::Format);
    for _ in 0..2 {
        d.handle_key(&key(KeyCode::Right));
    }
    assert_eq!(d.format(), Some(image::ImageFormat::Gif));
    assert!(!d.has_quality(), "a GIF has neither setting");
}

#[test]
fn the_prefix_and_postfix_are_typed_and_handed_back() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Prefix);
    for c in "small_".chars() {
        d.handle_key(&typed(c));
    }
    d.focus_control(Control::Postfix);
    for c in "-web".chars() {
        d.handle_key(&typed(c));
    }
    d.focus_control(Control::Ok);
    match d.handle_key(&key(KeyCode::Enter)) {
        DialogOutcome::Accept(DialogResult::Resize(settings)) => {
            assert_eq!(settings.prefix, "small_");
            assert_eq!(settings.postfix, "-web");
            assert!(settings.keep_ratio);
            assert_eq!(settings.width, Amount::Percent(50));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_size_is_refused_in_the_dialog_rather_than_started() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Width);
    for _ in 0..4 {
        d.handle_key(&key(KeyCode::Backspace));
    }
    d.focus_control(Control::Ok);
    assert!(matches!(
        d.handle_key(&key(KeyCode::Enter)),
        DialogOutcome::Consumed
    ));
    assert!(d.error().is_some());
}

#[test]
fn the_height_is_inert_while_the_ratio_is_kept_and_read_once_it_is_not() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Height);
    for _ in 0..4 {
        d.handle_key(&key(KeyCode::Backspace));
    }
    assert_eq!(
        d.settings().height,
        Amount::Percent(50),
        "the edit was refused rather than taken"
    );
    assert!(d.error().is_some(), "and it said why");
    d.focus_control(Control::Ok);
    assert!(
        matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::Resize(_))
        ),
        "with the ratio kept the height is not read at all"
    );

    // With the ratio off it is live, and an empty one is refused at `OK`.
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::KeepRatio);
    d.handle_key(&typed(' '));
    d.focus_control(Control::Height);
    for _ in 0..4 {
        d.handle_key(&key(KeyCode::Backspace));
    }
    d.focus_control(Control::Ok);
    assert!(matches!(
        d.handle_key(&key(KeyCode::Enter)),
        DialogOutcome::Consumed
    ));
    assert!(d.error().is_some());
}

#[test]
fn escape_cancels_and_alt_n_cancels() {
    let mut d = ResizeDialog::new(one_image());
    assert!(matches!(
        d.handle_key(&key(KeyCode::Esc)),
        DialogOutcome::Cancel
    ));
    let mut d = ResizeDialog::new(one_image());
    assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
}

#[test]
fn every_control_is_reachable_by_its_alt_letter() {
    let mut d = ResizeDialog::new(one_image());
    for (letter, want) in [
        ('w', Control::Width),
        ('e', Control::WidthUnit),
        ('g', Control::Height),
        ('p', Control::HeightUnit),
        ('i', Control::Fit),
        ('f', Control::Format),
        ('q', Control::Quality),
        // The two name checkboxes are gates: the letter ticks the box and
        // leaves the caret in the field it gates, which is the whole gesture.
        ('x', Control::Prefix),
        ('a', Control::Postfix),
    ] {
        d.handle_key(&alt(letter));
        assert_eq!(d.focused(), want, "Alt+{letter}");
    }
    assert!(d.prefix_is_on(), "Alt+X ticked the box as well");
    assert!(d.postfix_is_on(), "and so did Alt+A");
    // None of that stepped anything: a stepper is focus-only.
    let settings = d.settings();
    assert_eq!(settings.width, Amount::Percent(50));
    assert_eq!(settings.format, None);
    assert_eq!(settings.jpeg_quality, 85);

    let mut d = ResizeDialog::new(one_image());
    match d.handle_key(&alt('o')) {
        DialogOutcome::Accept(DialogResult::Resize(_)) => {}
        other => panic!("Alt+O pressed OK, got {other:?}"),
    }
}

#[test]
fn a_mnemonic_never_types_into_a_field() {
    let mut d = ResizeDialog::new(one_image());
    for letter in ['w', 'e', 'g', 'p', 'i', 'f', 'q', 'x', 'a', 'k'] {
        d.handle_key(&alt(letter));
        assert_eq!(d.settings().width, Amount::Percent(50), "Alt+{letter}");
        assert!(d.settings().prefix.is_empty(), "Alt+{letter}");
    }
}

#[test]
fn mnemonics_are_unique_within_this_dialog() {
    let mut seen: Vec<char> = Vec::new();
    for (control, letter) in MNEMONICS {
        assert!(letter.is_ascii_lowercase(), "{control:?}: stored folded");
        assert!(!seen.contains(letter), "{control:?}: Alt+{letter} is taken");
        seen.push(*letter);
        assert!(CONTROLS.contains(control), "{control:?} is not in the ring");
    }
    // Two controls deliberately have none: the name fields are reached
    // through the checkboxes that gate them, and a second letter for a field
    // would be a way in that leaves the box unticked and the typing ignored.
    assert_eq!(
        seen.len(),
        CONTROLS.len().saturating_sub(2),
        "every control but the two gated fields has a letter"
    );
    for control in [Control::Prefix, Control::Postfix] {
        assert!(
            !MNEMONICS.iter().any(|(c, _)| *c == control),
            "{control:?} is reached through its checkbox"
        );
    }
}

#[test]
fn every_mnemonic_is_underlined_in_its_own_label() {
    let d = ResizeDialog::new(one_image());
    let mut want = d.mnemonic_letters();
    let mut got = underlined(&d, 90, 20);
    want.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, want, "underlines on screen");
}

#[test]
fn the_unit_letters_stay_put_when_the_unit_is_stepped() {
    // `e` and `p` are the only letters `percent` and `pixels` share, and the
    // reason they were chosen: an underline that moves when a stepper is
    // stepped is a label that cannot be learned.
    let mut d = ResizeDialog::new(one_image());
    let before = {
        let mut letters = underlined(&d, 90, 20);
        letters.sort_unstable();
        letters
    };
    d.focus_control(Control::WidthUnit);
    d.handle_key(&key(KeyCode::Right));
    d.focus_control(Control::KeepRatio);
    d.handle_key(&typed(' '));
    d.focus_control(Control::HeightUnit);
    d.handle_key(&key(KeyCode::Right));
    let mut after = underlined(&d, 90, 20);
    after.sort_unstable();
    assert_eq!(after, before, "the same letters, in the same labels");
}

#[test]
fn the_dialog_draws_its_controls_and_says_what_they_hold() {
    let d = ResizeDialog::new(one_image());
    let rows = rendered(&d, 76, 12);
    let all = rows.join("\n");
    for wanted in [
        "holiday.png",
        "4000 x 3000 px",
        "PNG",
        "Keep aspect ratio",
        "Width:",
        "Height:",
        "percent",
        "Fit:",
        "Format:",
        "Quality:",
        "prefix filename",
        "append to filename",
        "Saved as:",
        "OK",
        "Cancel",
    ] {
        assert!(
            all.contains(wanted),
            "{wanted} is not on the screen:\n{all}"
        );
    }
}

#[test]
fn it_draws_inside_the_smallest_terminal_the_program_supports() {
    // 60x15 is the floor this program declares usable, and a dialog wider than
    // the terminal is a bug rather than a rounding error. Drawn through the
    // framework, borders and all, because the clamp lives there.
    let d = ResizeDialog::new(one_image());
    let backend = ratatui::backend::TestBackend::new(60, 15);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    for ascii in [false, true] {
        let style = DialogStyle::new(
            &crate::config::Theme::blue(),
            crate::config::ColorDepth::TrueColor,
            ascii,
        );
        terminal
            .draw(|f| {
                crate::dialog::draw(f, &d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let out: String = (0..15)
            .map(|y| {
                (0..60)
                    .filter_map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("Resize"), "no sign of the title:\n{out}");
        assert!(out.contains("Keep aspect"), "no controls:\n{out}");
        if ascii {
            assert!(out.is_ascii(), "a non-ASCII glyph at 60x15:\n{out}");
        }
    }
}

#[test]
fn the_one_call_queues_a_request_and_its_answer_becomes_a_job() {
    // The whole route, because a dialog that cannot be reached is not a
    // feature: one call captures the operands and queues the header read, the
    // event loop opens the dialog with what came back, and `dialog_accepted`
    // turns the answer into exactly one job aimed at the other panel's
    // directory.
    use crate::app::App;
    use crate::config::{Config, Keymap, Theme};
    use crate::input::{DialogId, dialog_accepted};
    use crate::ops::JobKind;
    use crate::vfs::{Entry, VfsPath};

    let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
    let tab = app.left.active_tab_mut();
    tab.path = VfsPath::local("/home/t/photos");
    tab.entries = vec![Entry::file("holiday.png")];
    app.right.active_tab_mut().path = VfsPath::local("/srv/media");

    crate::input::files::open_resize(&mut app);
    assert!(
        app.top_dialog().is_none(),
        "no dialog yet: the header has not been read, and dispatch may not read it"
    );
    let request = app.take_pending_resize().expect("a request was queued");
    assert_eq!(request.name, "holiday.png");
    assert_eq!(request.count, 1);

    // What the event loop does with the answer.
    app.push_dialog(Box::new(ResizeDialog::new(one_image())));
    let dialog = app.top_dialog().expect("the dialog opened");
    assert_eq!(dialog.id(), DialogId::Resize);
    let settings = dialog
        .as_any()
        .and_then(|any| any.downcast_ref::<ResizeDialog>())
        .expect("the resize dialog")
        .settings();

    dialog_accepted(
        &mut app,
        DialogId::Resize,
        DialogResult::Resize(Box::new(settings)),
    );
    let queued = app.take_pending_jobs();
    assert_eq!(queued.len(), 1, "one job, not none and not two");
    let spec = &queued[0].spec;
    assert_eq!(spec.kind, JobKind::Resize);
    assert_eq!(spec.sources.len(), 1);
    assert_eq!(
        spec.dest.as_ref().map(ToString::to_string),
        Some("/srv/media".to_string()),
        "the other panel's directory, as F5 and F6 do"
    );
    assert!(
        spec.options.resize.is_some(),
        "the settings travelled with it"
    );
}

#[test]
fn the_header_describes_one_image_and_counts_several() {
    let d = ResizeDialog::new(one_image());
    let header = d.header();
    assert!(header.contains("holiday.png"), "{header}");
    assert!(header.contains("4000 x 3000 px"), "{header}");
    assert!(header.contains("PNG"), "{header}");
    assert!(header.contains("11 MB"), "the size on disk: {header}");

    // Several are counted rather than described: a line about one of twelve
    // would be describing the wrong one eleven times.
    let d = ResizeDialog::new(many(12));
    assert_eq!(d.header(), "12 images");
}

#[test]
fn a_header_that_could_not_be_read_says_so_rather_than_refusing() {
    let d = ResizeDialog::new(ResizeSubject::unknown("mystery.img", 2048, 1));
    let header = d.header();
    assert!(header.contains("mystery.img"), "{header}");
    assert!(header.contains("dimensions unavailable"), "{header}");
    // And the dialog is fully usable: the size is still a size.
    let mut d = ResizeDialog::new(ResizeSubject::unknown("mystery.img", 2048, 1));
    d.focus_control(Control::Ok);
    assert!(matches!(
        d.handle_key(&key(KeyCode::Enter)),
        DialogOutcome::Accept(DialogResult::Resize(_))
    ));
}

#[test]
fn the_preview_shows_the_name_that_will_be_written() {
    // Nothing set: the name is the source's own. The source name is not
    // repeated here - the header line above says it - but the directory the
    // output lands in is, because nothing else in the dialog shows it.
    let mut d = ResizeDialog::new(one_image());
    assert!(
        d.preview().contains("Saved as: holiday.png"),
        "{}",
        d.preview()
    );
    assert!(
        d.preview().contains("in /photos/small"),
        "the preview does not say where the output goes: {}",
        d.preview()
    );

    // A prefix.
    d.handle_key(&alt('x'));
    for c in "thumb_".chars() {
        d.handle_key(&typed(c));
    }
    assert!(
        d.preview().contains("Saved as: thumb_holiday.png"),
        "{}",
        d.preview()
    );

    // A postfix as well.
    d.handle_key(&alt('a'));
    for c in "-web".chars() {
        d.handle_key(&typed(c));
    }
    assert!(
        d.preview().contains("Saved as: thumb_holiday-web.png"),
        "{}",
        d.preview()
    );

    // And the format change, which is the half that is otherwise only
    // visible by inference from the stepper.
    d.focus_control(Control::Format);
    for _ in 0..2 {
        d.handle_key(&key(KeyCode::Right));
    }
    assert_eq!(d.format(), Some(image::ImageFormat::Jpeg));
    assert!(
        d.preview().contains("Saved as: thumb_holiday-web.jpg"),
        "{}",
        d.preview()
    );
}

#[test]
fn an_unticked_box_means_no_affix_whatever_is_in_the_field_beside_it() {
    let mut d = ResizeDialog::new(one_image());
    d.focus_control(Control::Prefix);
    for c in "thumb_".chars() {
        d.handle_key(&typed(c));
    }
    assert!(d.prefix_is_on(), "typing a prefix is asking for one");
    assert_eq!(d.settings().prefix, "thumb_");

    // `Space` on the box takes it off again, and the text stays where it is
    // so that ticking it back does not mean typing it again.
    d.focus_control(Control::PrefixOn);
    d.handle_key(&typed(' '));
    assert!(!d.prefix_is_on());
    assert_eq!(d.settings().prefix, "", "the box is the answer");
    assert!(
        d.preview().contains("Saved as: holiday.png"),
        "{}",
        d.preview()
    );
}

#[test]
fn the_preview_says_the_rest_follow_the_same_rule() {
    let mut d = ResizeDialog::new(many(12));
    d.handle_key(&alt('a'));
    for c in "_small".chars() {
        d.handle_key(&typed(c));
    }
    let preview = d.preview();
    assert!(preview.contains("holiday_small.png"), "{preview}");
    assert!(preview.contains("11 more"), "{preview}");
}
