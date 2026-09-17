# Holos Commander - features

Every feature below is implemented and tested. Where a limit exists it is
stated, not hidden.

## Two-pane operations

- Two panels side by side, up to 9 tabs each.
- Configurable columns: name, extension, size, date, attributes, git state.
  Columns drop by priority as the panel narrows; name never drops.
- Sort by any column with a secondary key. `Ctrl+<n>` sorts by the n-th column
  as you have ordered them.
- Quick search by typing, incremental.
- Marking by key, by `*.rs`-style mask, by inversion, or by comparing the two
  panels. `Shift` with a movement key sweeps a range.
- Directory sizes on demand (`Space`), reported exactly.
- Hidden-files toggle, brief and full views, and bookmarked directories
  (`Ctrl+D`).
- Quick view (`Ctrl+Q`): the opposite panel previews the file under the cursor,
  following it as you move.
- Branch view (`Ctrl+B`): the whole tree below the current directory as one flat
  panel to search, mark and act on.
- `Ctrl+E` brings the other panel here; `Ctrl+P` puts the path on the command
  line.

## Local files

- Copy, move, rename, delete and make-directory on the classic function keys.
- Every long operation is a cancellable background job with progress;
  cancellation is honoured inside the copy loop, not only between files.
- Conflict handling per file or per batch: skip, overwrite, overwrite if newer,
  overwrite if a different size, rename, or refuse.
- Optional copy verification; data is synced to the medium before the
  destination is replaced.
- Delete to the desktop trash where one exists, with a permanent variant.
- Mode and timestamp preservation, best-effort with a warning where refused.
- Multi-rename with a pattern language, a preview and its own undo.
- Resize and convert images in bulk, carrying the source's pixel size, format
  and channel count through unchanged.
- Compare the two listings and mark differences by name, size and date
  (`Shift+F2`) or byte for byte (`Ctrl+Shift+F2`); copy the marks yourself with
  `F5`/`F6`.
- Compare the two files under the cursors byte for byte, for a same-or-differs
  verdict.
- Checksums: write and verify SHA-256 (`.sha256`) and CRC32 (`.sfv`), compatible
  with `sha256sum -c`.
- Symbolic and hard links, and permission editing, offered only where the
  backend supports them.
- Split a file into `name.001`, `name.002`, ... and merge them back.
- External editor (`F4`), with a size guard before a huge file is handed over.
- Copy the selection's full path to the system clipboard.

## Virtual filesystems

Enter archives, disk images, databases and remote hosts like directories. They
nest, and the type is decided by content, not by the extension.

**Archives**

- Browse into and copy out of ZIP, 7z, RAR and TAR (including `.tar.gz`,
  `.tar.xz`, `.tar.zst`, `.tar.bz2`). Write ZIP, the TAR family and 7z.
- A single-compressed file (`disk.img.xz`) opens as its one member.
- Listings stream as the index is built; nothing is read whole into memory.
- Entries whose names would escape the destination, and members that lie about
  their size, are refused and counted.

**Disk images** (read-only)

- ISO 9660 (Joliet, Rock Ridge), FAT12/16/32, ext2/3/4 and SquashFS, with GPT
  and MBR partition tables.
- exFAT, NTFS, HFS+ and APFS are recognised and named, not read.
- SquashFS covers AppImages, Snaps, initramfs and most router firmware.

**SQLite** (read-only, optional build feature)

- A `.db`/`.sqlite`/`.sqlite3`/`.db3` file opens as a directory of tables; a
  table is a directory of rows.
- Rows read and copy out as JSON (`F3`, `F5`); the table's own columns become
  panel columns you can sort by.
- Read through the SQLite library: WAL, overflow pages and `WITHOUT ROWID`
  tables are all handled.

**Remote**

- SFTP, FTP/FTPS, SMB2/3, S3 and WebDAV, all in process, with no `ssh` command
  or `libsmbclient`.
- Credentials from an agent, a key, or the system keyring; saved hosts in
  `hosts.toml`.
- A connected panel behaves like a local one: copy, move, view, search, and
  browse an archive that lives on it.
- SMB signing, encryption and share enumeration; S3 SigV4 with paged listings;
  WebDAV `MKCOL`, `MOVE` and `DELETE`.

## Search

- By name (glob or regex) and by content, over local trees, remote hosts and
  inside archives.
- Runs in process on ripgrep's own libraries; nothing is spawned.
- Results are a panel whose rows appear, and can be acted on, while the walk is
  still running.
- Optional `.gitignore` respect, off by default. Searches can be saved and
  reloaded.

## Viewer (`F3`)

- Streaming: a 40 GB file opens as fast as a 4 KB one, and memory is bounded by
  the window, not the file.
- Three modes, chosen by content and switchable with `1`/`2`/`3`: text, hex, and
  a document mode for JSON, HTML, Markdown and compiled Android XML.
- Syntax highlighting in the active theme; wrap toggle, tab width, configurable
  hex grouping.
- Find and find-next, shared with Find Files; in document mode it searches the
  rendered text.
- Selection including column selection, copied via OSC 52; encoding detection
  with a manual override.
- `i` in hex reads the bytes under the cursor as every integer width, float and
  timestamp, following the cursor field by field.
- Binary templates paint the regions they know in hex and name each field in the
  status bar.

## Git

- File status in the listing and the branch in the status line, read straight
  from the object store with no `git` process.
- History as directories (`Alt+V`): commits are folders named by short id and
  subject; enter one to list the files it changed.
- Diffs against `HEAD` (`Alt+D`) or between the two panels (`Alt+Shift+F2`),
  unified and coloured, with unchanged runs collapsed and expandable.

## Knowing what a file is

- `Shift+F9` on a panel, or `F9` in the viewer, describes the file: name, size,
  attributes and date, then what its bytes actually are.
- 109 built-in binary templates covering images, audio and video, executables
  and libraries, filesystems, fonts, firmware, bytecode and more, compiled into
  the binary.
- Templates report facts, not just fields: a PNG says `1920 x 1080 px`, `RGBA`;
  a WAV says `44.1 kHz`, `stereo`; an ELF says `x86-64`.

## Console

- `Ctrl+O` gives a persistent shell the whole screen and takes it back, with
  scrollback preserved.
- A terminal-holding command such as `git clone` takes the screen while it runs
  and hands the panels back when it finishes.
- The shell's directory and the active panel stay in step, both ways (OSC 7 and
  OSC 133 hooks for bash and zsh).
- A built-in command line with its own history stands in before a shell starts.

## Interface

- 21 themes plus a 16-colour fallback, with a truecolor/256/16 ladder chosen by
  detection.
- Live-preview theme picker (`Alt+T`) that also offers themes from the project
  repository, fetched on demand, and on an [Omarchy](https://omarchy.org/)
  desktop a dynamic `omarchy` theme that follows the desktop's colours live.
- Every command binding is rebindable per context in `keymap.toml`; the `F1`
  reference is generated from your keymap and marks any key this terminal cannot
  deliver.
- Menu bar, context menu, mnemonic dialogs, a job queue, mouse support and
  bracketed paste.
- Update check at startup and on demand (`Alt+U`): tells you once per version,
  with the install command. It downloads nothing and never replaces the binary.
- Works down to 60 columns, with an ASCII spelling of every piece of chrome for
  terminals without box drawing.

## Configuration

- Commented TOML in `~/.config/holoscommander/`, written self-documenting on
  first run. An unknown key is a warning with a line number, never a refusal to
  start.
- `hcmd --check-config` validates without starting; `hcmd --update-config`
  migrates an older file, keeping your values and comments and adding options
  that have since appeared.
- The reference file and the validator are generated from one walk of the config
  structs, so an option cannot reach the file without the validator knowing it.

## Deliberately not included

- No subprocesses for the program's own work; only the shell and the apps you
  open files with.
- No writing to disk images. Read-only, and not a step towards writing.
- No `.rar` creation.
- No configuration file rewritten behind you, beyond what you change through the
  UI: the hotlist, hosts and saved searches, the theme line, and the one line in
  `update.toml`.
