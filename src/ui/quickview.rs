//! the quick view: the viewer, drawn inside a panel.
//!
//! > The viewer, in the **other panel**, following the active panel's cursor.
//! > The same `Viewer` and the same streaming rules; what differs is only
//! > where it draws and that it re-opens as the cursor moves.
//!
//! So there is no second viewer here and no second row painter:
//! [`crate::viewer::Viewer`] is opened exactly as `F3` opens one, and
//! [`super::viewer::draw_body`] paints the rows. What this module owns is the
//! three rows of the panel that change - the column header becomes the file's
//! name, the entry rows become the file, the panel status line becomes the
//! viewer's - and the one case that is not a file at all: a directory under
//! the cursor, which shows its entry count and total size.
//!
//! **The panel keeps its path and cursor**, because it is still that panel.
//! The border, the path line, the volume line and the tab bar are drawn by
//! [`super::panelview::draw`] exactly as they always are.
//!
//! # Colours
//!
//! `viewer.*`, because it is the viewer (
//! the design). No new theme slot exists for a quick view
//! and none is wanted: a file shown in a panel should look like the same file
//! shown by `F3`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;
use crate::ops::walk::TreeStats;
use crate::panel::format;
use crate::vfs::VfsPath;

use super::text::{Crop, Glyphs};

/// What a directory under the cursor shows instead of a file.
///
/// > A directory under the cursor shows its entry count and total size rather
/// > than an error, and **re-uses the size walk of the `Ctrl+L`** rather
/// > than starting a second kind of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirSummary {
    /// The [`crate::ops::JobKind::Size`] walk is still running.
    Walking,
    /// It finished. The figures come straight out of
    /// [`crate::ops::walk::SizeCache`], which is the same cache `Ctrl+L` and
    /// `Space` fill and which [`crate::app::App::request_read`] invalidates.
    Done(TreeStats),
}

/// The quick view's whole state.
#[derive(Debug)]
pub struct QuickView {
    /// The panel **showing** the viewer. The panel it follows is
    /// `side.other()`.
    pub side: crate::panel::Side,
    /// The open viewer, when a file is showing.
    pub viewer: Option<crate::viewer::Viewer>,
    /// What is showing, so a cursor move that lands back on it is a no-op.
    pub subject: Option<VfsPath>,
    /// A directory's figures, when a directory is under the cursor.
    pub summary: Option<DirSummary>,
    /// The file the cursor is resting on, and since when.
    ///
    /// **Replaced, never queued**: held `Down` through a
    /// directory overwrites this on every keystroke and opens nothing until
    /// the cursor stops. "This is the same problem the key drain solves for
    /// input and it is solved the same way, by dropping what has been
    /// superseded."
    pub pending: Option<Pending>,
    /// The last error, shown in place of the body rather than as a dialog: a
    /// modal box per unreadable file while walking a directory is exactly what
    /// the debounce exists to prevent.
    pub error: Option<String>,
}

impl QuickView {
    /// A quick view showing in `side`, with nothing opened yet.
    ///
    /// The debounce is armed by [`crate::app::App::note_quick_view_cursor`],
    /// which the same keystroke that opened this calls, so an empty one lives
    /// for exactly as long as it takes the event loop to come round.
    pub const fn new(side: crate::panel::Side) -> Self {
        Self {
            side,
            viewer: None,
            subject: None,
            summary: None,
            pending: None,
            error: None,
        }
    }

    /// Forget whatever is showing, keeping the panel the quick view is in.
    ///
    /// Dropping the [`crate::viewer::Viewer`] cancels its background line
    /// index through the existing `ScanJob` cancel flag, which
    /// is why a file the cursor leaves costs nothing to leave.
    pub fn clear(&mut self) {
        self.viewer = None;
        self.subject = None;
        self.summary = None;
        self.error = None;
    }
}

/// A file the cursor is resting on (the debounce).
#[derive(Debug, Clone)]
pub struct Pending {
    /// What to open.
    pub path: VfsPath,
    /// When the cursor arrived. The deadline is this plus
    /// `viewer.quick_view_delay`.
    pub at: std::time::Instant,
    /// True for a directory, which takes the size walk instead of a viewer.
    pub is_dir: bool,
}

/// Draw the quick view into a panel's body.
///
/// `area` is the rectangle [`super::panelview::draw`] would have given the
/// entry rows, the tab bar and header excluded. The panel's own border, path
/// and volume line are untouched, because "the panel keeps its path and cursor
/// while it is showing a file".
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bg = Style::new()
        .fg(super::color(app, app.theme.viewer.fg))
        .bg(super::color(app, app.theme.viewer.bg));
    f.render_widget(Block::new().style(bg), area);

    let Some(quick) = app.quick.as_deref() else {
        return;
    };
    if let Some(viewer) = quick.viewer.as_ref() {
        super::viewer::draw_body(f, app, viewer, area);
        return;
    }
    // Everything else is one short paragraph in the middle of the body: a
    // directory's figures, a reason, or nothing at all while the debounce
    // runs. None of them is a dialog - the whole point is that
    // moving the cursor must stay cheap and quiet.
    let lines = message_lines(app, quick);
    if lines.is_empty() {
        return;
    }
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let width = usize::from(area.width);
    let body: Vec<Line> = lines
        .iter()
        .map(|text| {
            Line::from(Span::raw(super::text::fit_left(
                text,
                width,
                Crop::Middle,
                g.ellipsis(),
            )))
        })
        .collect();
    f.render_widget(Paragraph::new(body).style(bg), area);
}

/// The header row's text: the file being viewed, or the directory being
/// walked.
///
/// Middle-cropped, so the name survives a narrow panel: the head of a path is
/// the part every row shares and the tail is the part that says which file
/// this is.
pub fn header(app: &App) -> String {
    let Some(quick) = app.quick.as_deref() else {
        return String::new();
    };
    if let Some(viewer) = quick.viewer.as_ref() {
        return viewer.title().to_string();
    }
    match (quick.subject.as_ref(), quick.pending.as_ref()) {
        (Some(path), _) => path.to_string(),
        (None, Some(pending)) => pending.path.to_string(),
        (None, None) => String::new(),
    }
}

/// The status row's text: the viewer's own status fitted to a panel's width
/// ([`super::viewer::status_fit`]), or the directory's figures.
pub fn status(app: &App, width: usize) -> String {
    let Some(quick) = app.quick.as_deref() else {
        return String::new();
    };
    if let Some(viewer) = quick.viewer.as_ref() {
        return super::viewer::status_fit(&viewer.status(), width);
    }
    if let Some(error) = quick.error.as_ref() {
        return error.clone();
    }
    match quick.summary {
        Some(DirSummary::Walking) => WALKING.to_string(),
        Some(DirSummary::Done(stats)) => summary_line(app, stats),
        None => String::new(),
    }
}

/// What a directory shows while its walk is still running.
///
/// The verb [`crate::ops::JobKind::title`] already gives that walk
/// ("Calculating size"), lowercased for a line that is not a title, so the
/// quick view and the job queue describe the same work with the same word.
const WALKING: &str = "calculating...";

/// The lines drawn in the body when there is no viewer.
fn message_lines(app: &App, quick: &QuickView) -> Vec<String> {
    if let Some(error) = quick.error.as_ref() {
        return vec![error.clone()];
    }
    match quick.summary {
        Some(DirSummary::Walking) => vec![WALKING.to_string()],
        Some(DirSummary::Done(stats)) => vec![
            counts_line(stats),
            format::status_size(stats.bytes, &app.config.panel),
        ],
        // The debounce is running, or the panel is empty. A file that is about
        // to open must not flash a message on its way in.
        None => Vec::new(),
    }
}

/// `12 files, 3 dirs`, in the panel's own words.
fn counts_line(stats: TreeStats) -> String {
    format!(
        "{} file{}, {} dir{}",
        stats.files,
        if stats.files == 1 { "" } else { "s" },
        stats.dirs,
        if stats.dirs == 1 { "" } else { "s" },
    )
}

/// The one-line form the status row uses: the counts and the total together.
fn summary_line(app: &App, stats: TreeStats) -> String {
    format!(
        "{} in {}",
        format::status_size(stats.bytes, &app.config.panel),
        counts_line(stats)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};
    use crate::panel::Side;

    fn app() -> App {
        App::headless(Config::default(), Keymap::builtin(), Theme::blue())
    }

    #[test]
    fn a_directory_reports_its_counts_and_its_total_rather_than_an_error() {
        // "A directory under the cursor shows its entry count
        // and total size rather than an error."
        let mut app = app();
        let mut quick = QuickView::new(Side::Right);
        quick.summary = Some(DirSummary::Walking);
        app.quick = Some(Box::new(quick));
        assert_eq!(status(&app, 40), "calculating...");

        let stats = TreeStats {
            bytes: 4096,
            files: 12,
            dirs: 1,
        };
        if let Some(q) = app.quick.as_deref_mut() {
            q.summary = Some(DirSummary::Done(stats));
        }
        let line = status(&app, 40);
        assert!(line.contains("12 files"), "{line}");
        assert!(
            line.contains("1 dir") && !line.contains("1 dirs"),
            "singular, and not `1 dirs`: {line}"
        );
        assert!(
            line.contains(&format::status_size(4096, &app.config.panel)),
            "the panel's own byte formatting: {line}"
        );
    }

    #[test]
    fn an_unreadable_file_is_a_line_in_the_body_and_never_a_dialog() {
        // A modal box per unreadable file while walking a directory is exactly
        // what the debounce exists to prevent.
        let mut app = app();
        let mut quick = QuickView::new(Side::Right);
        quick.error = Some("permission denied".to_string());
        app.quick = Some(Box::new(quick));
        assert_eq!(status(&app, 40), "permission denied");
        let Some(q) = app.quick.as_deref() else {
            panic!("the quick view was just installed");
        };
        assert_eq!(
            message_lines(&app, q),
            vec!["permission denied".to_string()]
        );
    }

    #[test]
    fn nothing_is_drawn_while_the_debounce_is_still_running() {
        // A file about to open must not flash a message on its way in.
        //
        let mut app = app();
        app.quick = Some(Box::new(QuickView::new(Side::Right)));
        assert_eq!(status(&app, 40), "");
        assert_eq!(header(&app), "");
        let Some(q) = app.quick.as_deref() else {
            panic!("the quick view was just installed");
        };
        assert!(message_lines(&app, q).is_empty());
    }

    #[test]
    fn the_header_names_the_file_the_debounce_is_waiting_on() {
        let mut app = app();
        let mut quick = QuickView::new(Side::Right);
        quick.pending = Some(Pending {
            path: VfsPath::local("/home/t/notes.txt"),
            at: std::time::Instant::now(),
            is_dir: false,
        });
        app.quick = Some(Box::new(quick));
        assert_eq!(header(&app), "/home/t/notes.txt");
    }
}
