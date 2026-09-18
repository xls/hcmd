# Changelog

Notable changes per release, one line each. Newest first.

## v0.16.0

- `Alt+X` sends the selection to a LocalSend device: a native, send-only implementation of the protocol - devices found by multicast and a subnet scan, or a typed address; PIN when the device asks; runs as a job with a progress bar and cancel; encrypted to the device's own certificate, pinned to the fingerprint it announced. The picker says what is going (`Send folder - 12 files`) and warns past 50 files.
- The serve dialog stamps each answered request with the time.

## v0.15.0

- `dav://` connections work: the WebDAV client's HTTP library refused `PROPFIND`, `MKCOL` and `MOVE` as methods HTTP/1.1 does not define, so every WebDAV connect failed at its first listing; proven by hcmd connecting to its own `Ctrl+N` share.
- `Ctrl+N` listens on `serve.port` (8080; any free port when that is taken, and the dialog says so) so a firewall rule can name it; the dialog names the host firewall that is on - ufw, firewalld, nftables or iptables - says whether it blocks the port, and shows the command that opens it.
- The serve dialog has a Stop serving button, a ten-row request log, and a fixed size that truncates long lines instead of growing.
- In the rendered view the focused link is the one the cursor stands in: `Tab` finds the next link after the cursor and puts the cursor on it, and moving off a link drops its underline and marker.

## v0.14.0

- `Ctrl+N` serves the selected files and folders over HTTP and WebDAV for as long as its dialog is open: the dialog lists the addresses to reach it on and logs the last five requests; the index page is plain HTML with name, size and date, downloads resume, and another copy of hcmd can connect to it as a `dav://` remote.
- Links are underlined when the cursor stands on them, in the rendered view and over a bare URL in plain text, so what `Enter` will act on is visible.
- Mode 3 selects and copies the rendered text: `Shift` with the arrows, `Ctrl+A` and `Ctrl+C` take what is drawn, never the Markdown or HTML source; the cursor keeps its column across blank lines.

## v0.13.0

- A download manager: `Ctrl+D` fetches a URL into a per-session downloads folder (`Alt+J` opens it) as a background job with a progress bar, resumable and cancellable like a copy; a file already there asks Resume, Overwrite, Rename or Skip through the copy's own dialog.
- The viewer walks links: `Tab` and `Shift+Tab` move between them in the rendered view and over bare `http(s)://` in plain text; `Enter` downloads a file link or opens a page in the browser, `Shift+Enter` always opens the browser.
- The viewer has a key bar of its own along the bottom, naming the mode keys and its function keys as the keymap binds them.
- A copy installed with `npx holos-installer` is offered a self-update when a newer release is out; Skip waits for the next one.
- Keys moved: the change-drive fallbacks are `Alt+W` and `Alt+E`, the job queue's `Alt+B`; the hotlist lives in the drives popup under a divider, freeing `Ctrl+D` for downloads and `Alt+J` for the downloads folder.
- The update check runs once at startup (`ui.check_for_updates`) and blinks a notice on the right status bar until `Alt+U` dismisses it.
- The About page names the author and the repository, and thanks Christian Ghisler for Total Commander.

## v0.12.0

- On an [Omarchy](https://omarchy.org/) desktop, an `omarchy` theme paints the panel from the desktop's own colour scheme; it is built from the live palette rather than fetched from the repository, and appears in the `Alt+T` picker only where Omarchy is installed.
- The omarchy theme follows the desktop: changing the desktop theme recolours a running session in place, through a small `theme-set` hook hcmd installs itself and a `SIGUSR1` reload.

## v0.11.0

- The default theme is now tokyo-night; the blue theme and the other twenty still ship, `Alt+T` switches between them, and `HCMD_THEME` forces one for a single run.
- A SQLite table opens at once: its columns are known before the rows stream in, the rows page by rowid so a million-row table fills in linear time, each column is sized to its own values, and the first is the row id under the name the table gives it rather than "Name".
- The hex viewer's binary templates walk a format's chunks, so a WAV's format, sample rate and bit depth read correctly past a `JUNK` or `bext` chunk instead of as zero; the same walk serves AVI, WebP and other RIFF files.
- A ZIP whose sizes follow the data - a streamed archive, an APK - shows them as "deferred" rather than a misleading 0.
- Entering an archive that sits on a remote host no longer blanks the panel until the whole file has downloaded: the way out appears at once, the download shows a progress bar in the status line, and leaving cancels it.
- `PgUp` and `PgDn` within a page of the top or bottom of a file land on its first or last byte instead of doing nothing.
- The `F9` menu's dropdown fills the width of its box under a rule, rather than a narrow column hung under its title.
- A tab left inside an archive, disk image or database reopens in the folder that holds the file, not the home directory.

## v0.10.0

- Browse a SQLite database as directories: Enter a `.db`/`.sqlite` file to list its tables, enter a table to stream its rows, and each row reads as JSON with F3 and copies out as `<database>.<table>.<id>.json` with F5. Read-only.
- A table's own first columns become the panel's columns, so Ctrl+2 sorts by the second column of the database and Ctrl+3 by the third, like any other column.
- The SQLite backend is a default-on `sqlite` build feature; `cargo build --no-default-features` produces a pure-Rust binary without it, and everything else is unaffected.
- A listing can now define its own columns rather than only choosing among the panel's built-in ones, with per-row values that sort as numbers or text.
- A backend can suggest the format the viewer reads a row as, so a database row named `10000` still opens as highlighted JSON.

## v0.9.13

- Holding Ins in a large directory no longer skips rows or marks them twice: a rescan that finished while you were moving used to put the cursor back where it had been when the rescan started.
- A directory that changes while it is being read no longer starts a second read on top of the first every 200 ms.
- Shift with a movement key sweeps the mark, the way Total Commander does it: shift+up, shift+down, shift+pgup, shift+pgdn, shift+home and shift+end. The row the sweep starts on decides whether it marks or clears, and a partial page at either edge sweeps to the edge.

## v0.9.12

- Ctrl+A under a quick-search filter marks only the rows you can see, and F8 then deletes only those - it used to mark every file in the directory, hidden ones included.
- hcmd --update-config no longer comments out a setting written as `ui.theme = "nord"`, and leaves a file it cannot parse alone instead of rewriting every option at its default.
- --update-config keeps your `[terminal.sequences]` and `[viewer.highlight.lsp]` entries, which it used to delete.
- keymap.toml regeneration carries over a binding written under a different section than the shipped file declares it in, and leaves an unparsable keymap alone rather than silently commenting out every binding in it.
- The git column is drawn whenever `panel.git_status` is on rather than appearing and disappearing as you walk between repositories; a blank cell means the file is clean.
- A directory shows the state of what is under it, so `src` reads as modified when something inside it is.
- The git letters are the ones git itself uses: M modified, S staged, A added, U untracked, D deleted, R renamed.
- Each panel's status line names the branch it is on, right-aligned.
- Alt+V lists what a commit changed instead of the whole tree at that revision, each row saying what the commit did to it, and coming back out lands on the commit you left and in the folder you started from.
- A listing chooses its own columns: a commit list shows the name and date, a commit's files show the name, size, date and state.
- An APK's AndroidManifest.xml opens as the XML it was compiled from; 1 and 2 still show the file's own bytes.
- A UTF-16 file with no byte order mark opens as text rather than as a hex dump.
- The console gives the panels back when a command finishes instead of waiting for Ctrl+O.
- Ctrl+E brings the other panel to this panel's directory.
- The F9 menu is the size of the menu it shows.
- Alt+F5 offers only formats an archive can be created in, and no longer prefills a target from a panel that is not local.

## v0.9.11

- The panels now watch their own directory and refresh on their own when it changes underneath - a sync, an archive being packed, a file written by another program - instead of needing Ctrl+R.
- A refresh updates the listing in place rather than rebuilding it, so the git column and the status line no longer blank and repaint each time; rows keep their cursor, marks and flags, gone rows drop, and new ones appear.
- Cancelling a copy no longer pops a "1 failed - Retry?" dialog for the file it was interrupted on: a cancelled job is not a failed one.
- The copy dialog draws both progress bars at all times, so its height no longer twitches as it moves between differently sized files, and the box is a little wider.
- hcmd --update-config brings a file that already lists every option but carries an older stamp up to date, so --update-config and --check-config no longer disagree.

## v0.9.10

- A background copy or move now refreshes the destination panel the moment it finishes, instead of leaving a stale listing until the next Ctrl+R.

## v0.9.9

- Alt+F9 opens a background jobs dialog: a progress bar per job, an overall bar, and a small activity indicator in the panel's bottom-right corner while any work is running.
- In that dialog Del cancels and removes a job at once; a completed or cancelled job disappears on its own, and the dialog closes once nothing is left.
- A background task that blocks on a question is brought to the foreground on its own, so the prompt is on screen instead of waiting unseen in the queue.
- The size cell animates while a folder is being walked, in place of <DIR>; the style is configurable (panel.size_walk_style, off to keep <DIR>).
- The panel's top bar shows free space in human-readable units with the percentage in use, rather than a raw kilobyte count.
- hcmd --update-config appends commented examples of newly added options to an existing config without changing any of your settings, and the installer offers to run it.
- The configuration is reloaded as soon as the editor opened from the menu's "Edit configuration" is closed.
- Below its minimum screen size the app now says so, and its size, and lets you quit with Esc, F10 or Q instead of drawing a broken layout.

## v0.9.8

- Quick search can now filter the listing to matches as you type (panel.quick_search_filter, off by default); the arrows walk what is left and Esc brings the whole listing back.
- A marked file under the cursor is now legible: the cursor bar keeps its colour and the file takes a dark shade of the mark colour, tuned per theme.
- Four light themes get a higher-contrast mark colour, held there by a per-theme legibility test.
- A focused control's label in a dialog takes the mark colour instead of the list cursor bar, consistently across dialogs.
- Adding a host selects the new host rather than the Add button; a failed connection is now a dismissable dialog; Ctrl+F disconnect defaults to Yes.
- A keymap.toml from an older version is noticed at startup, and moving between a dialog's controls is rebindable through a [dialog] context.
- The quit prompt and the About page say "Holos Commander", and the About page names the version.

## v0.9.7

- A keymap.toml from an older version is now noticed at startup, so bindings added since do not silently do nothing.
- Moving between a dialog's controls (Tab and Shift+Tab) is now rebindable through a [dialog] keymap context.

## v0.9.6

- Compare Directories can now compare by content with Ctrl+Shift+F2, catching a file that differs without differing in size or date. Shift+F2 stays the quick size-and-date compare.

## v0.9.5

- A git-state column in the local listing: modified, staged, added and untracked flags, shown only inside a repository.
- Browse a directory's git history as a folder tree; open, view and diff any file at any commit.
- Warn before F4 opens a file larger than the configured limit.

## v0.9.4

- S3 backend: browse buckets, view keys, upload and download, bucket to bucket.
- WebDAV backend: browse, view, upload and download.
- Checksums: create and verify SHA-256 and CRC32 sidecars.
- Split and merge files into and from numbered parts.
- Create symlinks and hardlinks, and edit permissions.
- Bookmarks for the network protocols, with a password field and optional AWS environment variables.
- S3 speaks plain HTTP where asked, signs only when it has a key, and surfaces the endpoint's own error text.

## v0.9.3

- See what changed in the viewer: a diff mode offered alongside the file's own format.
- Contextual help pops up full screen on top, no longer squeezed into a dialog corner.
- The viewer's help page is generated from the keymap like every other page.

## v0.9.2

- The npm installer installs the latest release, and its version is kept in step with the crate.

## v0.9.1

- Viewer mode 3 searches the document it is drawing.
- Enter a container by what it is, not by what it is named.

## v0.9.0

- First public 0.9 release: the two-panel manager, viewer, archives, disk images, SSH/SFTP and SMB, the job engine, and the ten packaged targets.
