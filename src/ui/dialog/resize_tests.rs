//! Tests for [`super`].

use super::*;
use crate::input::{KeyModifiers, KeyPress};

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
    let d = ResizeDialog::new(3);
    let settings = d.settings();
    assert!(settings.keep_ratio);
    assert_eq!(settings.width, Amount::Percent(50));
    assert_eq!(settings.format, None, "same as source, until it is changed");
    assert_eq!(d.focused(), Control::KeepRatio);
}

#[test]
fn a_number_typed_into_the_width_reaches_the_settings() {
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
    d.focus_control(Control::Width);
    for c in "abc".chars() {
        assert!(matches!(d.handle_key(&typed(c)), DialogOutcome::Consumed));
    }
    assert_eq!(d.settings().width, Amount::Percent(50));
}

#[test]
fn space_toggles_keep_ratio_and_alt_k_only_ever_turns_it_on() {
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(2);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
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
    let mut d = ResizeDialog::new(1);
    assert!(matches!(
        d.handle_key(&key(KeyCode::Esc)),
        DialogOutcome::Cancel
    ));
    let mut d = ResizeDialog::new(1);
    assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
}

#[test]
fn every_control_is_reachable_by_its_alt_letter() {
    let mut d = ResizeDialog::new(3);
    for (letter, want) in [
        ('w', Control::Width),
        ('e', Control::WidthUnit),
        ('g', Control::Height),
        ('p', Control::HeightUnit),
        ('i', Control::Fit),
        ('f', Control::Format),
        ('q', Control::Quality),
        ('r', Control::Prefix),
        ('s', Control::Postfix),
    ] {
        d.handle_key(&alt(letter));
        assert_eq!(d.focused(), want, "Alt+{letter}");
    }
    // None of that stepped anything: a stepper is focus-only.
    let settings = d.settings();
    assert_eq!(settings.width, Amount::Percent(50));
    assert_eq!(settings.format, None);
    assert_eq!(settings.jpeg_quality, 85);

    let mut d = ResizeDialog::new(3);
    match d.handle_key(&alt('o')) {
        DialogOutcome::Accept(DialogResult::Resize(_)) => {}
        other => panic!("Alt+O pressed OK, got {other:?}"),
    }
}

#[test]
fn a_mnemonic_never_types_into_a_field() {
    let mut d = ResizeDialog::new(1);
    for letter in ['w', 'e', 'g', 'p', 'i', 'f', 'q', 'r', 's', 'k'] {
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
    assert_eq!(seen.len(), CONTROLS.len(), "every control has a letter");
}

#[test]
fn every_mnemonic_is_underlined_in_its_own_label() {
    let d = ResizeDialog::new(3);
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
    let mut d = ResizeDialog::new(1);
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
    let d = ResizeDialog::new(7);
    let rows = rendered(&d, 70, 10);
    assert!(rows.len() >= 7, "seven rows of controls");
    let all = rows.join("\n");
    for wanted in [
        "Keep aspect ratio",
        "Width:",
        "Height:",
        "percent",
        "Fit:",
        "Format:",
        "Quality:",
        "Prefix:",
        "Postfix:",
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
    let d = ResizeDialog::new(3);
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
fn the_one_call_opens_it_and_its_answer_becomes_a_job() {
    // The whole route, because a dialog that cannot be reached is not a
    // feature: one call opens it over the marked selection, and
    // `dialog_accepted` turns the answer into exactly one queued job aimed at
    // the other panel's directory.
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
    let dialog = app.top_dialog().expect("a dialog opened");
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
