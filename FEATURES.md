# Holos Commander - features

Everything below is implemented and covered by tests. Where a limit exists, it
is stated rather than glossed over.

## Panels

- Two panels side by side, each with up to 9 tabs.
- Columns: name, extension, size, date, attributes, and a one-character git
  state. Order and widths are configurable, and columns are dropped in a
  configured priority as the panel narrows. Name is never dropped.
- Git state in the listing: inside a repository each file carries a flag -
  modified, staged, newly added or untracked - and the column appears only
  where there is a repository to describe. On by default, and off turns the
  whole check off so an ordinary directory read never touches git.
- Sort by any column, ascending or descending, with a secondary sort. Positional
  binding: `Ctrl+<n>` sorts by the n-th column as *you* have ordered them.
- Quick search by typing. Matching is incremental and the status line says when
  nothing matched.
- Marking: by key, by `*.rs`-style mask, by inverting, and by comparing the two
  panels.
- Directory sizes on demand (`Space`), reported exactly.
- Hidden files toggle, brief and full views, and a hotlist of bookmarked
  directories (`Ctrl+D`).
- Quick view (`Ctrl+Q`): the opposite panel becomes a live view of the file
  under the cursor, following it as the cursor moves.
- Branch view (`Ctrl+B`): a flat, recursive listing of the whole tree under the
  current directory, as one panel of files to search, mark and act on.

## Files

- Copy, move, rename, delete, and make directory, on the classic function keys.
- View (`F3`) and edit (`F4`) in the external viewer and editor. Editing a file
  past a configurable size (10 MB by default) asks first, so a stray `F4` on a
  disk image does not hand it to an editor that would try to load the whole
  thing.
- Multi-rename with a pattern language, a preview, and its own undo.
- Conflict handling per file or for the whole batch: skip, overwrite, overwrite
  if newer, overwrite if a different size, rename, or refuse.
- Copies are verified on request, and are synced to the medium before the
  destination is replaced, so a truncated write is never reported as success.
- Deletion goes to the desktop trash where one exists, with a permanent variant.
- Attribute preservation for mode and timestamps, best effort with a warning
  where the filesystem refuses.
- Every long operation runs as a cancellable background job with progress, and
  cancellation is honoured inside the copy loop rather than only between files.
- Compare the two listings and mark what differs on either side, by name, size
  and date (`Shift+F2`), or by reading the bytes so a file changed without
  changing its size or date is still caught (`Ctrl+Shift+F2`). Marking is all
  it does: copy the marks whichever way you like with `F5` or `F6`, which is the
  synchronise, driven by hand.
- Compare the two files under the cursors byte for byte, for a verdict: the
  same file, or the offset at which they stop agreeing.
- Copy the full path of the selection to the system clipboard, for pasting into
  a terminal or another program.
- Checksums: write and verify SHA-256 (`.sha256`) and CRC32 (`.sfv`). The
  formats are the ones that already exist, so a file this writes verifies with
  `sha256sum -c` and one `sha256sum` wrote verifies here. A line naming a file
  outside the list's own directory is refused rather than followed.
- Create symbolic and hard links, and edit permissions. Each is refused before
  its dialog opens where the backend cannot do it, which is the rule the copy
  engine already follows: a question answered with a form and then refused is
  worse than one never asked. An archive member's mode is in its header and a
  FAT image has no modes at all, so neither offers the dialog.
- Split a file into numbered parts and merge them back. `name.001`, `name.002`,
  as every other tool writes them. Merging starts at the first part and stops
  at the first number missing rather than producing a short file.
- Resize and convert images, with the source's own pixel size, format and
  channel count carried through rather than promoted.

## Archives

Browse into an archive as though it were a directory, and copy files back out.
Archives nest: an archive inside an archive is extracted to a session cache and
cleaned up on exit.

The format is decided by the file's **content**, not its extension, so an
archive under a name no table knows - an `.apkm`, an `.epub`, a `.jar` - opens
on `Enter` like any other.

A singly compressed file is a container holding exactly one member: `disk.img.xz`
holds `disk.img`, which can be viewed, copied out, or stepped into as a disk
image in its own right. The member's size is read from the container where the
container states it, never by decompressing to find out.

| Format | Read | Write |
| --- | --- | --- |
| `.zip` | yes | yes |
| `.tar`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` | yes | yes |
| `.gz`, `.xz`, `.zst`, `.bz2` (single file) | yes | no (writing one means recompressing the whole file) |
| `.7z` | yes | yes |
| `.rar` | yes | no (the format is not ours to write) |

Listings stream as the index is built, so a 500,000-entry archive fills the
panel as it is read rather than after. Nothing reads an archive whole into
memory.

Entries whose names would escape the destination are refused before extraction
and counted, and the refusal names the entry. Declared sizes are treated as
claims: a member that lies about its size stops being decompressed rather than
stopping being read.

## Disk images (read-only)

`.iso` and `.img` browse like directories. Detection is by content, because an
extension says nothing about what is inside.

| Filesystem | Support |
| --- | --- |
| ISO 9660, with Joliet and Rock Ridge | read |
| FAT12, FAT16, FAT32 | read |
| ext2, ext3, ext4 | read: names, sizes, modes, owners, symbolic links. No timestamps - the reader exposes none |
| SquashFS | read: names, sizes, modes, owners, timestamps, and seekable files. Symbolic links and device nodes cannot be listed; the rows around them are, and the count is reported |
| GPT and MBR partition tables | read |
| exFAT, NTFS, HFS+, APFS | recognised and named, not read |

A partitioned image lists its partitions, and entering one is a step in the path
rather than a directory inside the image. An unsupported filesystem is reported
by name ("exFAT, not supported"), which is a different thing from a damaged
image, which is a different thing again from a file that is not an image.

SquashFS is what an AppImage, a Snap, an initramfs and most router firmware
are, so those open as directories now. NTFS stays out on purpose: the one crate
that reads it pulls in a dependency cargo reports as containing code a future
Rust will reject.

Read-only is the feature, not a stage of one: there is no write path to disable.

## Remote

- **SFTP** over SSH, in process (`russh`), with `known_hosts` checking including
  hashed entries, and host-key changes surfaced rather than silently accepted.
- **FTP** and FTPS, in process (`suppaftp`), with explicit and implicit TLS.
- **SMB2 and SMB3**, in process (`smb2`), pure Rust with no `libsmbclient` and
  no FFI: signing, encryption, and share enumeration. A share is the first
  component of the path, so `/` on the connection is the server and lists its
  shares, and `..` out of a share lands there. Anonymous and guest logins are
  never prompted; a domain is written `DOMAIN\user`.
- Credentials come from an agent, a key, or the system keyring where one is
  available; the program degrades to asking rather than failing when it is not.
- A connected panel behaves like any other: copy, move, view, search, and browse
  an archive that lives on the remote.
- Saved connections in `hosts.toml`.

- **S3**, over `s3://` for TLS or `s3+http://` for a plain-HTTP endpoint like a
  MinIO in a container - or just paste the `http://` or `https://` URL and the
  scheme decides. Credentials come from the connect line, a saved bookmark, or
  the AWS environment variables. Buckets are
  the root listing, prefixes are directories, and a key is a file. Requests are
  signed with SigV4, checked here against the specification's own worked
  example rather than against itself. Listings page, so a bucket larger than
  one reply still fills the panel.
- S3 has no directories and no rename, and both show: an empty directory is a
  zero-byte object whose key ends in `/`, which is the convention every other
  tool uses, and renaming copies and then deletes, server side, which is why
  renaming a large object is not instant. A single upload is held in memory
  and refused past 512 MB; multipart upload is not built.
- **WebDAV**, over `dav://`, `davs://`, or the `https://` URL you copied out of
  a browser. Listing, reading, writing, `MKCOL`, `DELETE` and server-side
  `MOVE` for rename. No locking: holding and refreshing a lock for as long as a
  panel is open is a background obligation this program does not otherwise
  have, and without it two writers can overwrite each other exactly as they can
  over FTP or on a share.

## Search

- By name (glob or regex) and by content, over local trees, remote connections
  and inside archives.
- Runs in process on ripgrep's own libraries: `ignore` for the walk,
  `grep-searcher` and `grep-regex` for matching. Nothing is spawned.
- Results are a panel. Rows appear as they are found and can be acted on while
  the walk is still running.
- Optional respect for `.gitignore`; off by default, because a file manager
  should find what is on the disk.
- Searches can be saved and reloaded.

## Viewer

- Three modes, chosen by content and switchable with `1`, `2` and `3`: text,
  hex, and a document mode that renders JSON, HTML and Markdown as a document
  rather than as source. A binary the templates recognise is rendered there
  too, as its fields; one they do not still shows what can be read of it.
- In hex mode a template paints the regions it knows, and stepping the cursor
  into one reads it out in the status bar: what the field is, and its value.
- **Git history as a directory.** `Alt+V` opens the repository's history:
  commits are folders named by their short id and subject, each browsable as
  the tree at that commit - real directories you enter, files you view with
  `F3`, diff with `Alt+D` against the previous commit, or copy out with `F5`.
  Read-only. `Alt+V` again leaves it. It reads the object store directly, no
  `git` process.
- **Diffs.** `Alt+D` shows the file's diff against `HEAD`, and `Alt+Shift+F2`
  diffs the file under each panel's cursor. Unified format, `+` and `-`
  coloured, unchanged runs collapsed behind `... 27 unchanged lines` and
  expandable.
- The diff lives in the document mode rather than in one of its own, so `1` and
  `2` still give the file's own text and bytes and find still searches what is
  on screen. **The file's own format wins**: a modified `.md` opens as
  markdown and `Alt+D` swaps to the diff, while a file no renderer claims -
  most source code - opens on the diff directly, there being no document for it
  to displace.
- The status line says what git knows: `git modified`, or `git unmodified` for
  a tracked file you have not touched, so a toggle that does nothing is
  explained rather than mysterious.
- Git's object store is read directly, loose objects and packfiles alike. No
  `git` process is started.
- Streaming: a 40 GB file opens as fast as a 4 KB one, and memory is bounded by
  the window rather than the file.
- Syntax highlighting (`syntect`) with the active theme.
- Find and find-next, with the last search shared with Find Files, so `F3` on a
  search result walks the matches inside it. Find searches whatever the mode is
  showing: the file's text or bytes in modes 1 and 2, and in document mode the
  rendered text itself - so a JSON key you can see is a key you can search for,
  in the form it is drawn rather than the form it is stored in.
- Encoding detection with a manual override ring.
- Selection, including column selection, and copy to the system clipboard via
  OSC 52.
- `i` in hex mode opens a reading of the bytes under the cursor: every width
  from one to eight, signed and unsigned, both byte orders, `f32`, `f64` and a
  timestamp where the number plausibly is one. It follows the cursor, so
  walking a header with the arrow keys reads it field by field with nothing to
  select first.
- Wrap toggle, tab width, and a configurable hex grouping.
- The mode a file opens in is decided by the file, not by the last one viewed,
  so a text file never opens in hex because the file before it was binary.
  Whether a recognised document opens in document mode is configurable.

## Knowing what a file is

`Shift+F9` on a panel, or `F9` in the viewer, describes the file under the
cursor: its name, size, attributes and date, and then what its contents turn
out to be.

The second half comes from 109 binary templates covering boot records and
partition tables, filesystems, executables and libraries, images, audio and
video, archives, fonts, virtual disks, firmware, bytecode, forensic artefacts
and a few ROM formats. They are compiled into the binary, so this works on a
machine with no configuration at all.

A template that carries a summary reports facts rather than fields: a PNG says
`1920 x 1080 px`, `RGBA`, `deflate`; a WAV says `44.1 kHz`, `stereo`; an AVI
names its codec from the FourCC; an ELF says `x86-64`, `shared object`; a Java
class says `Java 21`. Where a value has no name in the table the raw number is
shown rather than a guess, and a field the file ends inside says so.

A file no template recognises still gets its own facts and a line saying the
contents were not recognised, which is most files and is not a failure.

## Console

- `Ctrl+O` gives a persistent shell the whole screen and takes it back, with
  scrollback preserved across the switch.
- The shell's directory and the active panel stay in step, both ways, using
  OSC 7 and OSC 133 prompt hooks installed for bash and zsh.
- Before a shell starts (or where one cannot), a built-in command line with its
  own history stands in.
- The completion indicator needs a shell that can say when a command *starts*:
  zsh, or bash 4.4 and later. macOS ships bash 3.2 as `/bin/bash`, so under
  that shell everything works except the indicator. Its own default shell, zsh,
  is unaffected.
- The active file or path can be inserted at the caret, quoted for the shell
  exactly once.

## Interface

- 21 themes, plus a 16-colour fallback for terminals that need it, and a
  truecolor/256/16 ladder chosen by detection.
- `Alt+T` previews each theme as the cursor moves over it, applies it on
  `Enter` and writes it into `config.toml` so it survives a restart. `Esc` puts
  back the one you started with.
- The picker lists your own `themes/*.toml` beside the built-in ones, then adds
  - marked with a trailing `+` - the themes the project's repository has and
  this machine does not. `Enter` on such a name fetches it, checks it parses as
  a theme before writing anything, writes it into `themes/`, and applies it. A
  machine with no network simply sees the themes it already has, and every
  network failure is a line in the status bar and nothing more.
- Every binding is rebindable per context in `keymap.toml`.
- The `F1` reference is *generated from your keymap*, so it shows your bindings,
  and marks any that this terminal cannot deliver alongside the fallback that
  works.
- A menu bar, a context menu, dialogs with mnemonic accelerators, and a job
  queue.
- Alt+U asks GitHub whether there is a newer release and tells you once per
  version, with the exact command that installs it. It downloads nothing and
  never replaces the binary.
- Mouse support, bracketed paste, and a panic hook that always restores the
  terminal.
- Works down to 60 columns, with an ASCII spelling of every piece of chrome for
  terminals without box drawing.

## Configuration

TOML in `~/.config/holoscommander/`, written commented-out on first run so the
file documents itself and every default is visible. An unknown key is a warning
with a line number, never a refusal to start. `hcmd --check-config` validates
without starting.

## Deliberately not included

- **No subprocesses for the program's own work.** Search, archives, device
  enumeration and file associations all run in process. The only processes
  started are the shell you asked for and the editor or application you opened a
  file with.
- **No writing to disk images.** Read-only, and not as a first step towards
  writing.
- **No `.rar` creation.** The format is not ours to write.
- **No configuration file that the program rewrites behind you**, apart from
  what you change through the UI: the three lists (hotlist, hosts, saved
  searches), the one line in `update.toml` noting which release you have been
  told about, and the theme, which `Alt+T` writes into `config.toml` as a single
  line. Your comments, spacing and every other setting are left exactly as you
  wrote them.
