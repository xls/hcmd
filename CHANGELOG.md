# Changelog

Notable changes per release, one line each. Newest first.

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
