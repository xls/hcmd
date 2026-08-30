//! The Multi-Rename Tool.
//!
//! `Ctrl+M`. the tree puts the multi-rename engine in `rename/`, and
//! this is that engine: four control groups' worth of model, a preview that
//! decides every status, a two-phase execution, and a session undo. The
//! painted half is [`crate::ui::dialog::multirename`] and
//! [`crate::ui::dialog::renameresult`].
//!
//! # The pipeline, in this order
//!
//! the design fixes it, and the order is the contract:
//!
//! 1. **expand the name mask** against [`mask::Context`] to get the stem;
//! 2. **expand the extension mask** to get the extension;
//! 3. **search and replace** the stem, and the extension too when the `[E]`
//!    toggle is on;
//! 4. **the case dropdown**, over the stem and the extension both;
//! 5. **join**: `stem` when the extension is empty, `stem.ext` otherwise.
//!
//! Replace comes **after** expansion, so it corrects the new name rather than
//! the old one; the other order would make `[N]` re-insert exactly what the
//! replacement had just removed. No dot is added when the extension expands
//! empty, so `[E]` on a file with no extension does not produce a trailing
//! dot.
//!
//! # What it operates on
//!
//! "Operates on the marked entries, or the whole directory if nothing is
//! marked." That is deliberately **not** the `F5`/`F8` rule, which falls back
//! to the row under the cursor - `Tab::rename_rows` is the rule and
//! `Tab::operand_rows` is the, and they are two functions so that nobody
//! unifies them. `..` is never one of them.
//!
//! # The four rules the whole module keeps
//!
//! * **Nothing here reads the disk except [`exec`].** [`plan::Plan::build`] is
//!   pure and is rebuilt on every keystroke.
//! * **A row's directory is its real home's parent, never the panel's path.**
//!   On a search result those differ, and collisions, `[P]` and the temporary
//!   names of [`exec`] are all consequences of that one rule.
//! * **Nothing panics.** No `unwrap`, no `expect`, no slice indexing: every
//!   index goes through `get`, every arithmetic step saturates, and every mask
//!   range is checked.
//! * **A rename never overwrites.** In both phases and in the recovery pass,
//!   existence is checked immediately before the rename.

pub mod exec;
pub mod mask;
pub mod plan;
pub mod replace;
pub mod saved;
pub mod state;

pub use exec::{ResultLine, Undo};
pub use mask::{Counter, MAX_NAME_BYTES, Mask};
pub use plan::{Plan, PreviewColumn, RenameItem, RenameStatus, Settings};
pub use replace::{Case, Replace};
pub use saved::SavedRename;
pub use state::MultiRename;
