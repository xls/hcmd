# Working on Holos Commander

A map for anyone - person or agent - making a first change here. It says what
each module owns, which rules are load-bearing rather than stylistic, and where
the traps are. Read the section for the area you are touching; you do not need
the whole thing.

## Ground rules

Run this before you claim anything works, and before you commit:

```sh
./scripts/gate.sh gate-green
```

It is `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, a
macOS type-check, and `cargo test`, in that order, with the results parsed
rather than eyeballed.

### The macOS step, and why it is not optional

Development happens on Linux, and `cargo test` on Linux never compiles the
macOS side of a `#[cfg]`. Three separate pushes have been red for the same
reason: a use of `trash::Error::FileSystem`, which exists only on the
freedesktop backend. Each time the first report was a CI failure minutes after
the push.

So `gate-green` cross-checks both Apple targets. No Mac and no Apple SDK are
involved: `cargo check` for a darwin target otherwise fails on Linux because
four dependencies compile C and `cc` cannot target darwin, and Zig ships the
macOS libc headers that make it possible.

One-time setup:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
mise use -g zig@0.15.1
cargo install cargo-zigbuild --locked
```

Run it alone with `./scripts/gate.sh macos-check`. It takes a couple of
seconds once warm. If it reports a missing tool it fails rather than skipping,
because a check that quietly does nothing is worse than no check.

When an API differs across platforms, do not gate the call site. Write one
function per platform with the same signature, each under its own `#[cfg]`,
and let the caller stay platform-blind: `trash_itself_unusable` in
`src/ops/delete.rs` is the pattern to copy.

Clippy denies the panic paths in production code: no `unwrap`, no `expect`, no
`panic!`, no `unreachable!`, no `Cell` or `RefCell`. `unsafe` is *forbidden*,
which is stronger - it cannot be re-enabled by an attribute. Test code is
exempt via `clippy.toml`, not via attributes, so write natural assertions in
tests and never add an `allow` to a test. In production use `?` with context,
`.get()`, and `let ... else`.

Two lints are `allow` rather than `deny`, and both say so in `Cargo.toml` with
the reason and the remaining count: `indexing_slicing`, for a handful of
sites not yet rewritten, and `disallowed_macros`, for the one `tokio::select`
that is the event loop itself. Do not read those as permission - new code is
written as though both were denied, and the counts are meant to fall. This
paragraph used to claim indexing was denied outright, which sent people
looking for an enforcement that was not there.

Every `#[expect]` or `#[allow]` needs `reason = "..."` naming the invariant that
makes it safe. "clippy is wrong" is not a reason.

Style that the tooling enforces and that reviewers will bounce:

- No em-dashes and no emoji, anywhere. Use ` - `.
- No references to a specification document. It is not in the tree.
- Never insert an item between a doc comment and the thing it documents;
  `missing_docs` is denied and the error will point somewhere confusing.
- Comments say *why*. The code already says what. Match the density of the file
  you are in.

## The shape of the program

```
    terminal
       |  key events
       v
  input::dispatch ......... decides what a key MEANS. Pure: no I/O at all.
       |  Action / request
       v
   runtime::event_loop .... services queued work, then draws one frame
       |                         |
       |  reads/writes           |  render
       v                         v
      vfs                       ui
```

Three things follow from that diagram, and most bugs in this codebase come from
breaking one of them:

1. **`dispatch` performs no I/O.** Not a `stat`, not a read, nothing. It turns a
   key into an intention and queues it. This is what makes ~2,100 tests able to
   drive the whole program without a terminal or a filesystem.
2. **The thread that draws must never block.** Blocking work goes to
   `spawn_blocking` or a named thread and comes back through a channel. A
   blocking call on the event loop freezes the UI with no way out - and a file
   manager spends its life on network mounts that hang.
3. **Everything that touches a file goes through `Vfs`.** A local path, a
   member of a zip, a file on an SFTP host and a file on a partition of a disk
   image are all the same thing to the layers above.

## Modules

| Module | Lines | Owns |
| --- | --- | --- |
| `src/vfs/` | 24k | The `Vfs` trait and its backends. The most important abstraction here. |
| `src/ui/` | 25.6k | Rendering. Takes state, draws widgets, owns no state of its own. |
| `src/viewer/` | 18k | The file viewer: text, hex, highlighting, find, streaming. |
| `src/remote/` | 15.2k | SFTP (russh) and FTP (suppaftp), connection state, host keys, secrets. |
| `src/ops/` | 13.7k | Copy, move, delete, pack, compare. The job engine and its progress. |
| `src/app/` | 9.6k | `App`, the state the whole program shares, plus the `service_*` methods. |
| `src/input/` | 6.9k | `dispatch`: key to `Action`, per context. No I/O. |
| `src/panel/` | 6k | A panel: its tabs, cursor, marks, sort, columns, quick search. |
| `src/config/` | 4.8k | TOML config, keymap, themes. Warnings, never hard failures. |
| `src/search/` | 4.8k | Name and content search over any backend. |
| `src/dialog/`, `src/ui/dialog/` | 4k + | The `Dialog` trait and every dialog. |
| `src/console/` | 3.9k | The `Ctrl+O` shell: pty, scrollback, prompt hooks. |
| `src/rename/` | 3.3k | Multi-rename with its own undo. |
| `src/term/` | 1.9k | Terminal setup, teardown, keyboard protocol detection, panic hook. |
| `src/devices/` | 1.2k | Mount table and the directory hotlist. |
| `src/net/` | small | The only outbound HTTP: the update check and the theme catalogue. Nothing else in the program opens a connection it was not asked to. |

Newer pieces worth knowing about, because they are not obvious from the table:

| Where | What |
| --- | --- |
| `src/viewer/template.rs`, `template_read.rs` | The binary struct template format, its parser, and `field_at` for the renderer. |
| `templates/` | 109 templates in fourteen directories, compiled into the binary through `template_data.rs`. A new file needs a line there to be built in. |
| `src/viewer/summary.rs`, `summary_render.rs` | The `[summary]` layer: enumerations, units, FourCC, flags - what turns a number into a fact. |
| `src/viewer/fileinfo.rs` | `describe`: the file's own facts plus what a template makes of its head. `HEAD_BYTES` is how much a caller must read. |
| `src/viewer/inspect.rs` | The bytes under the cursor, read every way at once. |
| `src/config/catalogue.rs` | Themes offered by the repository, fetched and validated before anything is written. |
| `src/config/persist.rs` | Writing one setting back into `config.toml` without disturbing the rest of the file. |
| `src/app/update.rs` | The update check and the once-per-version acknowledgement. |
| `src/app/fileinfo.rs` | The queued read behind the file-information dialog. |

`src/main.rs` is argument parsing and terminal setup. `src/runtime.rs` holds the
event loop and the async orchestration, in the library so that integration tests
can reach it.

## The `Vfs` trait, and how to add a backend

```rust
pub trait Vfs: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>>;
    fn stat(&self, path: &VfsPath) -> Result<Entry>;
    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn Read + Send>>;
    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn Write + Send>>;
    fn create_dir(&self, path: &VfsPath) -> Result<()>;
    fn remove(&self, path: &VfsPath) -> Result<()>;
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()>;
    fn capabilities(&self) -> Capabilities;

    // Defaulted. A backend implements one only where it can do better than
    // the refusal the default gives.
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> { .. }
    fn read_link(&self, path: &VfsPath) -> Result<String> { .. }
    fn capabilities_for(&self, path: &VfsPath) -> Capabilities { .. }
}
```

Backends: `LocalFs`, `ListFs` (a synthetic listing, used for search results and
drive lists), `ArchiveFs`, `RemoteFs`, `ImageFs`.

`read_dir` returns a **channel, not a Vec**, because a listing must appear as it
is found: an archive index, a remote walk and a search all fill a panel while
they are still running. Send the `..` row first and unconditionally, so a panel
whose listing failed still has the row that gets the user out.

### `VfsPath` is a stack of segments

```
/a/backup.img#/2#/boot/vmlinuz
[(Local, /a/backup.img), (Image, /2), (Image, /boot/vmlinuz)]
     the container         partition       the file on it
```

Each segment names a backend and a path *in that backend's namespace*. Entering
a container pushes a segment; `..` out of one pops it. Two segments of the same
kind can stack and mean different things, which is how a partition is a step in
the path rather than a directory inside one.

`Capabilities` is what the UI asks *before* offering an operation, so that `F5`
into a read-only backend is refused up front with a reason instead of failing
halfway through a copy.

### Adding a backend

1. Add a `BackendKind` variant. Every match on it is exhaustive by design, so the
   compiler will list what needs an arm. Add arms rather than a wildcard.
2. Implement `Vfs`. Return honest `Capabilities`; under-promising costs a
   refusal the user can retry, over-promising costs a half-finished copy.
3. Route it in `VfsRouter::backend_for`.
4. Names from a backend are **untrusted input**. Anything that is not the local
   filesystem gets its names from somebody else, so run them through
   `is_plain_name` at ingest, and member paths through
   `vfs::archive::safety::normalize_member`. This is what stops a listing that
   answers `../../.bashrc` from writing there. Do not fork the check; the point
   of one copy is that one fix is one fix.

## Where things live when you add a feature

**A new key or a new binding.** `src/input/action.rs` (the `Action` enum),
`examples/keymap.toml` (the default and its comment), `src/input/*.rs` (the
context that handles it). The `F1` page is generated from the keymap, so it
updates itself. Remember the `Alt`+letter fallback if the key is one a legacy
terminal cannot deliver.

**A new dialog.** Implement `Dialog` in `src/ui/dialog/`, add a `DialogId`, and
handle its answer in `src/input/dialogs.rs`. Mnemonics come from a per-dialog
table, not from `&` markers in labels.

**A new operation.** `src/ops/`. It runs as a job with progress and cancellation;
copy an existing one's shape. The cancel flag must be checked *inside* the work
loop, not only at the top - a cancel that is only honoured between files is not
a cancel on a 40 GB one.

**A new setting.** `src/config/config.rs` (the struct and its default),
`examples/config.toml` (the commented-out entry that documents it). An unknown
key is a warning with a line number, never a hard failure: a config that fails
to parse must not stop the program starting.

**A new archive format.** Implement `ArchiveFormat` in `src/vfs/archive/`.
Implementations are zero-sized and stateless; the index, safety checks and
storage belong to the layer above, which is what keeps the rules in one place
for every format.

## Traps

These have each cost real time.

- **`Write::flush` on a `std::fs::File` does nothing.** It returns `Ok(())` and
  the bytes are still in the page cache. Dropping the handle discards `close(2)`,
  which is where a network or quota-backed filesystem reports its failure. Use
  `ops::copy::commit_partial`: sync while the handle is owned, then rename. A
  copy that skips this reports success on a truncated file, and a *move* then
  deletes the source.
- **A test that filters can pass on an empty set.** `cargo test somefilter` with
  no matching tests prints `ok. 0 passed`. Any check built on that is certifying
  nothing. Assert a count.
- **A test that loops must assert the collection's length first**, or a function
  that returns nothing at all passes it.
- **A skip guard must not share its predicate with the bug.** `if
  !summary.failures.is_empty() { return; }` makes every real assertion
  unreachable exactly when the feature broke.
- **Cancellation tests must cancel mid-scan.** Setting the flag before the scan
  starts only tests the entry guard, which is not the bug that matters.
- **A dialog is not the only reader of a key.** Focus decides who consumes it,
  and a viewer or console consumes everything. Check the context tables.
- **Sizes and dates from a remote or an archive are claims, not measurements.**
  Never allocate from one.

## Testing

~2,100 unit tests in `src/`, plus integration suites in `tests/` that drive the
real binary over a pty and read the screen back with `vt100`.

`tests/remote_sshd.rs` needs a real sshd and is gated: `HCMD_SSHD_TEST=1 cargo
test --test remote_sshd -- --ignored`.

When you fix a bug, the test for it must fail without the fix. Check that by
reverting the fix, running the test, and putting the fix back. A test that
passes either way documents nothing.
