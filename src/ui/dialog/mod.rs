//! The v0.2 operation dialogs.
//!
//! Five dialogs live here, all against the one [`crate::dialog::Dialog`] trait
//! and the framework in [`crate::dialog`] - there is no second framework:
//!
//! * [`CopyMoveDialog`] - the design, the `F5`/`F6` dialog, modelled on the
//!   Total Commander screenshot the section is taken from.
//! * [`ProgressDialog`] - the two bars, two rates, `Esc` and `F2`.
//! * [`ConflictDialog`] - the six choices and their "all" variant.
//! * [`QueueDialog`] - the queue view: pending, active and failed jobs.
//! * [`SummaryDialog`] - the end-of-batch failure summary and its retry.
//! * [`RenameDialog`] - the `F2`: the filename alone, stem
//!   preselected, an existing name refused before anything happens.
//!
//! # Why these are in `ui::dialog` and not in `dialog`
//!
//! the design hands `src/dialog/**`'s *framework* to phase 1 and
//! its *primitives* - message, confirm, input - to everyone. These five are
//! neither: they are the painted half of the design, they are the only things
//! in the crate that draw a progress bar, and three agents were writing under
//! `src/dialog/` concurrently. They implement the same trait, they are
//! constructed the same way, and [`crate::input::dialog_accepted`] receives
//! them through the same [`crate::dialog::DialogResult`].
//!
//! # How a job dialog answers
//!
//! [`crate::dialog::DialogResult`] is fixed by the contract and has no
//! job-shaped variant, so the three dialogs that act on a *running job* answer
//! with [`crate::dialog::DialogResult::Text`] carrying a [`JobAction`].
//! [`JobAction::parse`] turns it back into a typed value, so the arm in
//! `input::dialog_accepted` is a `match` and not string handling:
//!
//! ```
//! use holoscommander::ops::JobId;
//! use holoscommander::ui::dialog::JobAction;
//!
//! let text = JobAction::Background(JobId(3)).encode();
//! assert_eq!(JobAction::parse(&text), Some(JobAction::Background(JobId(3))));
//! ```
//!
//! # The rules every dialog in here keeps
//!
//! * **It lays out against the rectangle it is given.** Every one is tested
//!   at 200x50, 80x24 and 60x15, and every `Rect`
//!   built here is checked for zero width or height first.
//! * **Every glyph has an ASCII fallback** under `ui.ascii_borders`.
//!   The bars, the spinner, the separators and the ellipsis are
//!   all declared in this file, so there is one place to audit.
//! * **Colour comes from the `dialog.*` slots only**, through
//!   [`DialogStyle`].

pub mod conflict;
pub mod context;
pub mod copy_move;
pub mod drives;
pub mod execute;
pub mod field;
pub mod fileinfo;
pub mod find;
pub mod history;
pub mod menu;
pub mod multirename;
pub mod openwith;
pub mod pack;
pub mod progress;
pub mod queue;
pub mod rename;
pub mod renameresult;
pub mod resize;
pub mod summary;
pub mod tabbed;
pub mod template;
pub mod theme;

use std::fmt;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::PanelConfig;
use crate::dialog::DialogStyle;
use crate::ops::JobId;
use crate::panel::format::{human_size, thousands};
use crate::ui::text;

pub use conflict::ConflictDialog;
pub use context::{ContextChoice, ContextItem, ContextMenuDialog};
pub use copy_move::CopyMoveDialog;
pub use drives::{DriveRow, DrivesDialog};
pub use execute::{ExecuteChoice, ExecuteDialog};
pub use field::Field;
pub use find::FindDialog;
pub use history::HistoryDialog;
pub use menu::{Menu, MenuDialog, MenuItem, MenuModel};
pub use multirename::MultiRenameDialog;
pub use openwith::OpenWithDialog;
pub use progress::{ProgressDialog, Smoother};
pub use queue::QueueDialog;
pub use rename::RenameDialog;
pub use renameresult::RenameResultDialog;
pub use summary::SummaryDialog;
pub use tabbed::{TabStrip, Table, TableColumn};

// ----------------------------------------------------------------- glyphs ---

/// The crop marker: `…`, or `...` under `ui.ascii_borders`.
pub const fn ellipsis(ascii: bool) -> &'static str {
    if ascii { "..." } else { "\u{2026}" }
}

/// A filled cell of a progress bar: `█`, or `#`.
pub const fn bar_full(ascii: bool) -> &'static str {
    if ascii { "#" } else { "\u{2588}" }
}

/// An empty cell of a progress bar: `░`, or `.`.
///
/// `.` rather than `-`: a row of hyphens reads as a rule, and the two halves of
/// a bar have to be told apart at a glance on a 16-colour terminal where they
/// may quantize to neighbouring greys.
pub const fn bar_empty(ascii: bool) -> &'static str {
    if ascii { "." } else { "\u{2591}" }
}

/// The four spinner phases, Unicode and ASCII (a slow walk shows
/// a spinner rather than blocking the dialog).
const SPINNER: [&str; 4] = ["\u{25D0}", "\u{25D3}", "\u{25D1}", "\u{25D2}"];
/// The ASCII spinner.
const SPINNER_ASCII: [&str; 4] = ["|", "/", "-", "\\"];

/// How long one spinner phase lasts.
const SPINNER_PHASE: u128 = 120;

/// The spinner glyph for an elapsed time.
///
/// A pure function of the elapsed time so it is testable without a clock and
/// cannot disagree with itself between two renders of the same frame.
pub fn spinner(elapsed: Duration, ascii: bool) -> &'static str {
    let set = if ascii { SPINNER_ASCII } else { SPINNER };
    let phase = elapsed
        .as_millis()
        .checked_div(SPINNER_PHASE)
        .unwrap_or(0)
        .rem_euclid(4);
    let index = usize::try_from(phase).unwrap_or(0);
    set.get(index).copied().unwrap_or("*")
}

/// A checkbox: `[x] Verify` or `[ ] Verify`.
///
/// The box is a **mark** and not part of the label, which matters because the
/// ON mark is the literal character `x`: see
/// [`crate::dialog::split_mnemonic`], where the two are told apart so a ticked
/// box cannot take the underline off a label whose mnemonic is `x`.
pub fn checkbox(label: &str, on: bool, ascii: bool) -> String {
    // `x` in both glyph sets: a checkbox is the one control whose ASCII form is
    // already the conventional one, and a one-cell mark cannot disturb a
    // column the way a two-cell tick would.
    let _ = ascii;
    let mark = if on { 'x' } else { ' ' };
    format!("[{mark}] {label}")
}

// ------------------------------------------------------------------ text ----

/// Crop `s` to `max` columns **from the left**, keeping its tail.
///
/// "Show the tail of the path, cropped from the *left* with an ellipsis so the
/// filename and its nearest parents survive - the same grapheme-cluster and
/// display-width rules, never a byte slice." Neither
/// [`crate::ui::text::truncate`] direction does this, because no panel column
/// needs it: a column keeps the head or both ends, and only a path being
/// written right now wants its last few components.
pub fn crop_left(s: &str, max: usize, ascii: bool) -> String {
    if max == 0 {
        return String::new();
    }
    if text::width(s) <= max {
        return s.to_string();
    }
    let marker = ellipsis(ascii);
    let marker_w = text::width(marker);
    if max <= marker_w {
        // No room for the marker and anything worth marking; the tail alone is
        // more use than a bare `…`.
        return text::take_back(s, max);
    }
    let tail = text::take_back(s, max.saturating_sub(marker_w));
    format!("{marker}{tail}")
}

/// A byte count formatted exactly as the `size` column would format it, so the
/// dialogs and the panel never disagree about what a byte count looks like.
///
pub fn bytes_text(bytes: u64, cfg: &PanelConfig) -> String {
    if cfg.human_sizes {
        human_size(bytes)
    } else if cfg.thousands_separator {
        thousands(bytes)
    } else {
        bytes.to_string()
    }
}

/// A transfer rate: `12.4 M/s`, or `-` when there is not enough of one to show
/// honestly.
///
/// A rate keeps one decimal at every magnitude, unlike the `size` column, which
/// drops it above ten to stay narrow. A rate is not a column and
/// has no width to defend, and `12 M/s` versus `13 M/s` is the difference
/// between two transfers a user is actively comparing - the own
/// example is `12.4 M/s`.
pub fn rate_text(rate: Option<u64>, ascii: bool) -> String {
    match rate {
        Some(bytes) => format!("{}/s", rate_size(bytes)),
        None if ascii => "-".to_string(),
        None => "\u{2014}".to_string(),
    }
}

/// A byte count with one decimal at every magnitude: `12.4 M`, `1.1 G`.
fn rate_size(value: u64) -> String {
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if value < 1024 {
        return format!("{value} B");
    }
    // A transfer rate is a figure to read at a glance, not an accounting total:
    // the f64 mantissa is exact past 8 EiB/s, and everything here is divided
    // down to at most four significant digits before it is shown anyway.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a rate is read at a glance, and the mantissa is exact past 8 EiB/s"
    )]
    let mut scaled = value as f64 / 1024.0;
    let mut unit = UNITS.first().copied().unwrap_or("K");
    for next in UNITS.iter().skip(1) {
        if scaled < 1024.0 {
            break;
        }
        scaled /= 1024.0;
        unit = *next;
    }
    format!("{scaled:.1} {unit}")
}

/// A duration as a clock: `00:34`, or `01:02:03` once it passes an hour.
pub fn clock_text(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (
        secs.checked_div(3600).unwrap_or(0),
        secs.checked_div(60).unwrap_or(0).rem_euclid(60),
        secs.rem_euclid(60),
    );
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

// ------------------------------------------------------------------ bars ----

/// The narrowest bar worth drawing. Below this it is one or two cells that
/// cannot show a fraction, so the caller drops the row instead.
pub const MIN_BAR_WIDTH: u16 = 8;

/// How many columns the ` 100 %` suffix needs.
const PERCENT_WIDTH: usize = 6;

/// A progress bar plus its percentage, exactly `width` columns wide.
///
/// `None` is a bar with no denominator (the `Size` job): an empty
/// trough and no number, rather than a bar that lurches.
pub fn bar_text(fraction: Option<f64>, width: usize, ascii: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let show_percent = width > PERCENT_WIDTH.saturating_add(usize::from(MIN_BAR_WIDTH));
    let track = if show_percent {
        width.saturating_sub(PERCENT_WIDTH)
    } else {
        width
    };
    let filled = match fraction {
        // `f64 -> usize` through a clamped fraction: the value is in 0..=track
        // by construction, so the cast cannot be lossy in a way that matters.
        Some(f) => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the fraction is clamped to 0.0..=1.0 on this line"
            )]
            let cells = (f.clamp(0.0, 1.0) * track as f64).round() as usize;
            cells.min(track)
        }
        None => 0,
    };
    let mut out = String::with_capacity(width.saturating_mul(3));
    for _ in 0..filled {
        out.push_str(bar_full(ascii));
    }
    for _ in filled..track {
        out.push_str(bar_empty(ascii));
    }
    if show_percent {
        match fraction {
            // `f` is clamped to 0.0..=1.0 on the line below, so the product is
            // 0..=100 and neither truncation nor sign loss is reachable.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the fraction is clamped to 0.0..=1.0, so the product is 0..=100"
            )]
            Some(f) => {
                let pct = (f.clamp(0.0, 1.0) * 100.0).round() as u64;
                out.push_str(&format!("{pct:>4} %"));
            }
            None => out.push_str("      "),
        }
    }
    out
}

/// Draw one progress bar into `area`, filled part and trough in two colours.
///
/// Both colours are `dialog.*` slots: the filled part takes
/// `dialog.button_focus`, which is the theme's "this is the live thing" colour,
/// and the trough takes `dialog.border`.
pub fn draw_bar(f: &mut Frame, area: Rect, fraction: Option<f64>, style: &DialogStyle) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let body = bar_text(fraction, usize::from(area.width), style.ascii);
    let full_char = bar_full(style.ascii).chars().next();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let filled = Style::new().fg(style.button_focus).bg(style.bg);
    let trough = Style::new().fg(style.border).bg(style.bg);
    // Split at the first cell that is not a filled block, so the two halves are
    // two spans rather than one span per cell.
    let mut head = String::new();
    let mut tail = String::new();
    let mut in_head = true;
    for g in body.chars() {
        if in_head && Some(g) == full_char {
            head.push(g);
        } else {
            in_head = false;
            tail.push(g);
        }
    }
    if !head.is_empty() {
        spans.push(Span::styled(head, filled));
    }
    if !tail.is_empty() {
        spans.push(Span::styled(tail, trough));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(style.body()), area);
}

/// Draw two pieces of text on one row, one flush left and one flush right.
///
/// The right-hand piece is dropped whole rather than truncated when the row is
/// too narrow for both: half a byte count is a wrong byte count.
pub fn draw_split(f: &mut Frame, area: Rect, left: &str, right: &str, style: Style, ascii: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = usize::from(area.width);
    let right_w = text::width(right);
    let line = if right_w > 0 && right_w.saturating_add(2) <= width {
        let room = width.saturating_sub(right_w);
        let head = text::fit_left(left, room, text::Crop::End, ellipsis(ascii));
        format!("{head}{right}")
    } else {
        text::truncate(left, width, text::Crop::End, ellipsis(ascii))
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, style))).style(style),
        area,
    );
}

/// One row of `area`, or `None` when the row is off the bottom of it.
///
/// Every rectangle these dialogs build goes through here, which is how
/// the "no `Rect` without checking for zero width or height" is kept
/// in one place rather than at forty call sites.
pub fn row(area: Rect, index: u16) -> Option<Rect> {
    if area.width == 0 || index >= area.height {
        return None;
    }
    Some(Rect::new(
        area.x,
        area.y.saturating_add(index),
        area.width,
        1,
    ))
}

// ------------------------------------------------------------ job actions ---

/// What a dialog asks be done to a *job*.
///
/// [`crate::dialog::DialogResult`] is fixed by the design and has
/// no job-shaped variant, so these travel as
/// [`crate::dialog::DialogResult::Text`] and come back through [`Self::parse`].
/// The encoding is deliberately boring - a verb, a space, the id - so a test
/// can assert on it and a log line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobAction {
    /// `F2` on the progress dialog: send it to the background queue, keep it
    /// running. [`crate::app::App::background_job`].
    Background(JobId),
    /// `Esc` on the progress dialog: stop it.
    /// [`crate::app::App::cancel_job`].
    Cancel(JobId),
    /// `Enter` in the queue view: bring it back "exactly as it was".
    /// [`crate::app::App::foreground_job`].
    Foreground(JobId),
    /// Drop a finished job from the queue view.
    /// [`crate::app::App::forget_job`].
    Forget(JobId),
    /// the "option to retry" the failures of a finished job.
    Retry(JobId),
}

impl JobAction {
    /// The verb half of the encoding.
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Background(_) => "background",
            Self::Cancel(_) => "cancel",
            Self::Foreground(_) => "foreground",
            Self::Forget(_) => "forget",
            Self::Retry(_) => "retry",
        }
    }

    /// Which job it is about.
    pub const fn id(&self) -> JobId {
        match self {
            Self::Background(id)
            | Self::Cancel(id)
            | Self::Foreground(id)
            | Self::Forget(id)
            | Self::Retry(id) => *id,
        }
    }

    /// The wire form: `background 3`.
    pub fn encode(&self) -> String {
        format!("{} {}", self.verb(), self.id().0)
    }

    /// Read one back. `None` for anything that is not one of these - which is
    /// every other dialog's `Text` answer, so an arm that calls this can share
    /// a match with them.
    pub fn parse(text: &str) -> Option<Self> {
        let (verb, id) = text.split_once(' ')?;
        let id = JobId(id.trim().parse::<u64>().ok()?);
        match verb {
            "background" => Some(Self::Background(id)),
            "cancel" => Some(Self::Cancel(id)),
            "foreground" => Some(Self::Foreground(id)),
            "forget" => Some(Self::Forget(id)),
            "retry" => Some(Self::Retry(id)),
            _ => None,
        }
    }
}

impl fmt::Display for JobAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_action_survives_a_round_trip_through_text() {
        for action in [
            JobAction::Background(JobId(1)),
            JobAction::Cancel(JobId(0)),
            JobAction::Foreground(JobId(42)),
            JobAction::Forget(JobId(7)),
            JobAction::Retry(JobId(u64::MAX)),
        ] {
            let text = action.encode();
            assert_eq!(JobAction::parse(&text), Some(action), "{text}");
        }
        // Every other dialog's Text answer must not look like one of these.
        assert_eq!(JobAction::parse("*.bak"), None);
        assert_eq!(JobAction::parse("/srv/media/*.*"), None);
        assert_eq!(JobAction::parse("background"), None);
        assert_eq!(JobAction::parse("background x"), None);
        assert_eq!(JobAction::parse("photos 2026"), None);
    }

    #[test]
    fn a_bar_is_exactly_the_width_it_is_given() {
        for width in 0usize..40 {
            for fraction in [None, Some(0.0), Some(0.43), Some(1.0)] {
                for ascii in [false, true] {
                    let bar = bar_text(fraction, width, ascii);
                    assert_eq!(
                        text::width(&bar),
                        width,
                        "{width} cols, {fraction:?}, ascii={ascii}: {bar:?}"
                    );
                    if ascii {
                        assert!(bar.is_ascii(), "{bar:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_full_bar_is_full_and_an_empty_one_is_empty() {
        let full = bar_text(Some(1.0), 20, true);
        assert!(full.starts_with("##############"), "{full:?}");
        assert!(full.ends_with("100 %"), "{full:?}");
        let empty = bar_text(Some(0.0), 20, true);
        assert!(empty.starts_with(".............."), "{empty:?}");
        assert!(empty.ends_with("0 %"), "{empty:?}");
        // No total, no number.
        let unknown = bar_text(None, 20, true);
        assert!(!unknown.contains('%'), "{unknown:?}");
        assert!(!unknown.contains('#'), "{unknown:?}");
    }

    #[test]
    fn a_path_is_cropped_from_the_left_so_the_filename_survives() {
        // twenty files called index.html, and the basename does
        // not say which one.
        let path = "/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf";
        let out = crop_left(path, 30, false);
        assert_eq!(text::width(&out), 30, "{out:?}");
        assert!(out.ends_with("POWER PANEL.3mf"), "{out:?}");
        assert!(out.starts_with('\u{2026}'), "{out:?}");

        let ascii = crop_left(path, 30, true);
        assert!(ascii.is_ascii(), "{ascii:?}");
        assert!(ascii.starts_with("..."), "{ascii:?}");

        // Short enough to fit is left alone, and a hopeless width still yields
        // something rather than panicking.
        assert_eq!(crop_left("a.txt", 30, false), "a.txt");
        assert_eq!(crop_left(path, 0, false), "");
        assert!(text::width(&crop_left(path, 2, false)) <= 2);
        assert!(text::width(&crop_left(path, 1, false)) <= 1);
    }

    #[test]
    fn a_wide_grapheme_never_overflows_a_left_crop() {
        // display width, never bytes. Two-cell CJK against an odd
        // budget is where a byte slice would land mid-character.
        let path = "/srv/媒体/文件名前缀/報告書.txt";
        for max in 1usize..30 {
            let out = crop_left(path, max, false);
            assert!(text::width(&out) <= max, "{max}: {out:?}");
        }
    }

    #[test]
    fn the_spinner_cycles_and_falls_back_to_ascii() {
        let phases: Vec<&str> = (0..8)
            .map(|i| spinner(Duration::from_millis(i * 120), true))
            .collect();
        assert_eq!(phases[0], "|");
        assert_eq!(phases[4], "|", "four phases, then it repeats");
        assert!(phases.iter().all(|p| p.is_ascii()));
        assert!(!spinner(Duration::ZERO, false).is_ascii());
    }

    #[test]
    fn a_clock_reads_as_a_clock() {
        assert_eq!(clock_text(Duration::from_secs(34)), "00:34");
        assert_eq!(clock_text(Duration::from_secs(89)), "01:29");
        assert_eq!(clock_text(Duration::from_secs(3723)), "01:02:03");
        assert_eq!(clock_text(Duration::ZERO), "00:00");
    }

    #[test]
    fn a_rate_is_never_fabricated() {
        // below the sample floor it renders as an em dash.
        assert_eq!(rate_text(None, false), "\u{2014}");
        assert_eq!(rate_text(None, true), "-");
        assert_eq!(rate_text(Some(13_002_342), false), "12.4 M/s");
        assert_eq!(rate_text(Some(900), false), "900 B/s");
        assert_eq!(rate_text(Some(1_200_000_000), false), "1.1 G/s");
    }

    /// Prints every dialog in this module at three sizes, for eyeballing a
    /// layout change. Not an assertion: the assertions are in each dialog's own
    /// module, and this is what tells a human whether the result looks right.
    #[test]
    #[ignore = "visual aid: cargo test -- --ignored --nocapture ui::dialog::tests::visual_dump"]
    fn visual_dump() {
        use crate::config::{ColorDepth, PanelConfig, Theme};
        use crate::dialog::Dialog;
        use crate::ops::{
            ConflictRequest, JobEvent, JobFailure, JobKind, JobStatus, JobSummary, SelectionStats,
        };
        use crate::vfs::VfsPath;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::SystemTime;

        fn show(d: &dyn Dialog, w: u16, h: u16, ascii: bool) {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
            terminal
                .draw(|f| {
                    crate::dialog::draw(f, d, f.area(), &style);
                })
                .expect("draw");
            let buf = terminal.backend().buffer().clone();
            println!("--- {} {w}x{h} ascii={ascii} ---", d.title());
            for y in 0..h {
                let line: String = (0..w)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect();
                let line = line.trim_end();
                if !line.is_empty() {
                    println!("{line}");
                }
            }
        }

        let cfg = PanelConfig {
            human_sizes: true,
            ..PanelConfig::default()
        };
        let stats = SelectionStats {
            bytes: 19_058_360_320,
            files: 523,
            dirs: 95,
            unsized_dirs: 0,
        };
        let copy = CopyMoveDialog::new(JobKind::Move, 3, "/srv/media/*.*", stats, &cfg);

        let mut status = JobStatus::queued(JobId(3), JobKind::Copy);
        status.apply(&JobEvent::Progress {
            file: "/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf".to_string(),
            file_bytes_done: 17_200_000,
            file_bytes_total: 40_000_000,
            files_done: 10,
            files_total: 200,
            bytes_done: 2_100_000_000,
            bytes_total: 8_700_000_000,
            throughput: Some(13_002_342),
            eta: Some(Duration::from_secs(89)),
            elapsed: Duration::from_secs(34),
        });
        let progress = ProgressDialog::new(status.clone(), 1024 * 1024);

        let now = SystemTime::now();
        let conflict = ConflictDialog::new(
            JobId(1),
            Box::new(ConflictRequest {
                source: VfsPath::local("/tmp/report.txt"),
                dest: VfsPath::local("/srv/media/report.txt"),
                source_size: 2_345_678,
                dest_size: 1_234_567,
                source_mtime: Some(now),
                dest_mtime: now.checked_sub(Duration::from_secs(86_400)),
                both_dirs: false,
                dest_is_dir: false,
            }),
            "report (2).txt",
            &PanelConfig::default(),
        );

        let done = JobSummary {
            kind: JobKind::Copy,
            files_done: 197,
            dirs_done: 3,
            bytes_done: 1_000,
            skipped: 0,
            failures: vec![JobFailure {
                path: VfsPath::local("/srv/media/deep/tree/a.txt"),
                error: "Permission denied (os error 13)".to_string(),
            }],
            cancelled: false,
            elapsed: Duration::from_secs(12),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        };
        let mut failed = JobStatus::queued(JobId(4), JobKind::Copy);
        failed.apply(&JobEvent::Finished {
            summary: Box::new(done.clone()),
        });
        let queue = QueueDialog::new(vec![status, failed]);
        let summary = SummaryDialog::new(JobId(4), done);

        let dialogs: [&dyn Dialog; 5] = [&copy, &progress, &conflict, &queue, &summary];
        for d in dialogs {
            for (w, h) in [(100u16, 30u16), (60, 15)] {
                show(d, w, h, false);
            }
            show(d, 100, 30, true);
        }
    }

    #[test]
    fn a_row_off_the_bottom_is_none_rather_than_a_zero_height_rect() {
        let area = Rect::new(2, 3, 20, 2);
        assert_eq!(row(area, 0), Some(Rect::new(2, 3, 20, 1)));
        assert_eq!(row(area, 1), Some(Rect::new(2, 4, 20, 1)));
        assert_eq!(row(area, 2), None);
        assert_eq!(row(Rect::new(0, 0, 0, 5), 0), None);
    }
}
