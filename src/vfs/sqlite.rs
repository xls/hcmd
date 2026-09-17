//! Browsing a SQLite database as directories.
//!
//! `db.sqlite#sqlite/` lists the tables and views. Entering one lists its
//! rows, streamed a page at a time so a table of millions costs no more to
//! open than a table of ten. A row reads as JSON: `F3` shows it, `F5` copies
//! it to the other panel as `<db>-<table>-<id>.json`.
//!
//! The tables come with columns of their own. The listing hands the panel a
//! [`ColumnPlan`] naming the table's first columns - `id`, `firstname`,
//! `lastname` - so `Ctrl+<n>` sorts by them, and each row carries its values
//! in [`Entry::cells`]. The panel draws, widens and sorts them knowing none of
//! the names.
//!
//! Read-only, entirely and forever. Every method that would change the
//! database refuses, the way the git backend refuses to rewrite history. The
//! reading is `rusqlite`'s - the C library the design admits by name - so the
//! WAL, overflow pages and `WITHOUT ROWID` tables are its problem, not this
//! module's.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::panel::text::Align;
use crate::panel::{ColumnId, ColumnPlan, CustomColumn};
use crate::vfs::{
    BackendKind, CellValue, Entry, EntryKind, READ_DIR_CHANNEL_DEPTH, ReadSeek, Vfs, VfsPath,
};

/// How many of a table's own columns become sortable columns in the panel.
///
/// The first few, because a listing has room for a handful beside the name and
/// the rest are read in the row's JSON. Five covers the `id, first, last,
/// project, date` shape a person sorts by without crowding the name off.
const MAX_TABLE_COLUMNS: usize = 5;

/// How many rows one page of a table's listing reads.
///
/// The stream is unbounded - a table of millions lists in full - but it is read
/// in pages so the first rows reach the panel while the rest are still coming,
/// and so a listing dropped early stops reading rather than running to the end.
const PAGE: usize = 512;

/// One open database, read-only.
#[derive(Debug, Clone)]
pub struct SqliteFs {
    /// The database file on disk, the outermost segment of every path here.
    file: PathBuf,
}

/// Where a `#sqlite` path points: the table list, or one table's rows.
enum Location {
    /// `#sqlite/` itself: every table and view.
    Tables,
    /// `#sqlite/<table>`: the rows of one table.
    Rows(String),
}

impl SqliteFs {
    /// Open the database the path's outer segment names, read-only.
    ///
    /// Refused when the outer segment is not local - a database is a file on
    /// the disk - or when the file is not a database `rusqlite` can open.
    pub fn open(display: VfsPath) -> Result<Self> {
        let file = match display.segments().first() {
            Some((BackendKind::Local, path)) => path.clone(),
            _ => return Err(Error::msg("a database is opened from a local file")),
        };
        // Opened and queried once here only to prove it is a database and to
        // fail with a clear message if it is not: `open` alone does not read
        // the file, so a plain text file would pass and only fail at the first
        // listing. Every listing opens its own connection, because a
        // `Connection` is not `Sync` and the reads run off the event loop.
        // Read-only, and `NO_MUTEX` because nothing here shares one.
        let conn = Self::connect(&file)?;
        conn.prepare("SELECT name FROM sqlite_master LIMIT 1")
            .and_then(|mut stmt| stmt.query([]).map(|_| ()))
            .map_err(|e| Error::msg(format!("{}: not a database ({e})", file.display())))?;
        Ok(Self { file })
    }

    /// A fresh read-only connection to the database.
    fn connect(file: &Path) -> Result<Connection> {
        Connection::open_with_flags(
            file,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| Error::msg(format!("{}: not a database ({e})", file.display())))
    }

    /// The `#sqlite` tail of a path, or the empty path for one without a
    /// sqlite segment.
    fn tail_of(path: &VfsPath) -> PathBuf {
        path.segments()
            .iter()
            .rev()
            .find(|(kind, _)| *kind == BackendKind::Sqlite)
            .map(|(_, tail)| tail.clone())
            .unwrap_or_default()
    }

    /// Read a `#sqlite` tail as a table list or a table's rows.
    fn locate(path: &VfsPath) -> Location {
        let tail = Self::tail_of(path);
        let text = tail.to_string_lossy();
        let name = text.trim_matches('/');
        if name.is_empty() {
            Location::Tables
        } else {
            Location::Rows(name.to_string())
        }
    }

    /// Every table and view in the database, as directory rows.
    fn list_tables(&self) -> Result<Vec<Entry>> {
        let conn = Self::connect(&self.file)?;
        // The catalogue, minus SQLite's own bookkeeping tables, which a person
        // browsing their data did not put there and cannot read as rows.
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .map_err(|e| Error::msg(format!("reading the table list: {e}")))?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| Error::msg(format!("reading the table list: {e}")))?;
        let mut out = Vec::new();
        for name in names {
            let name = name.map_err(|e| Error::msg(format!("a table name: {e}")))?;
            let mut entry = Entry::dir(name.clone());
            entry.kind = EntryKind::Dir;
            // The table opens by name, not by a path-join that would have to
            // quote it; the row's home says which table it is.
            entry.location = Some(
                VfsPath::local(&self.file).with_segment(BackendKind::Sqlite, format!("/{name}")),
            );
            out.push(entry);
        }
        Ok(out)
    }

    /// The columns a table's listing shows: its own first columns, named so
    /// the panel can sort by them, after the name that carries the row id.
    fn table_columns(&self, table: &str) -> Result<(ColumnPlan, Vec<String>)> {
        let conn = Self::connect(&self.file)?;
        let has_rowid = table_has_rowid(&conn, table);
        // A single-column INTEGER primary key *is* the rowid, so the row's name
        // already carries its value. It is the Name column's header, not a
        // column of its own that would print the id twice.
        let alias = rowid_alias(&conn, table, has_rowid);
        let names = column_names(&conn, table)?;
        let shown: Vec<String> = names
            .iter()
            .filter(|n| Some(n.as_str()) != alias.as_deref())
            .take(MAX_TABLE_COLUMNS)
            .cloned()
            .collect();
        let mut columns = vec![ColumnId::Name];
        let mut custom = Vec::new();
        for (i, name) in shown.iter().enumerate() {
            columns.push(ColumnId::Custom(u8::try_from(i).unwrap_or(u8::MAX)));
            custom.push(CustomColumn {
                header: name.clone(),
                align: Align::Left,
                min_chars: data_width(name),
            });
        }
        // A table is a grid: every column is sized to itself and packed from the
        // left, rather than one column stretching across the panel and leaving a
        // gap. The first column is the row id, headed by the name the table
        // gives it - an INTEGER primary key reads as `id`, not "Name" - and kept
        // as narrow as an id needs.
        let header = alias
            .clone()
            .unwrap_or_else(|| ColumnId::Name.header().to_string());
        let name = Some(CustomColumn {
            header,
            align: Align::Left,
            min_chars: id_width(alias.as_deref().unwrap_or("")),
        });
        Ok((
            ColumnPlan {
                columns,
                custom,
                name,
                flex: None,
                pack: true,
            },
            shown,
        ))
    }

    /// Read a table's rows into `tx`, a page at a time.
    ///
    /// The row name is its id - the rowid where the table has one, its position
    /// otherwise - so two rows never collide and `F5` writes a file named for
    /// it. Each row carries its first columns' values as cells and never the
    /// whole record; the record is what `F3` reads, from the JSON `open_read`
    /// builds.
    fn stream_rows(&self, table: String, tx: &mpsc::Sender<Result<Entry>>) {
        let conn = match Self::connect(&self.file) {
            Ok(conn) => conn,
            Err(err) => {
                let _ = tx.blocking_send(Err(err));
                return;
            }
        };
        let shown = match self.table_columns(&table) {
            Ok((_, shown)) => shown,
            Err(err) => {
                let _ = tx.blocking_send(Err(err));
                return;
            }
        };
        let has_rowid = table_has_rowid(&conn, &table);
        let mut offset = 0usize;
        loop {
            let page = match read_page(&conn, &table, &shown, has_rowid, offset) {
                Ok(page) => page,
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };
            let count = page.len();
            for entry in page {
                if tx.blocking_send(Ok(entry)).is_err() {
                    return;
                }
            }
            if count < PAGE {
                return;
            }
            offset = offset.saturating_add(count);
        }
    }

    /// One row of a table, as pretty JSON, addressed by the row's own name.
    fn row_json(&self, table: &str, row_name: &str) -> Result<Vec<u8>> {
        let conn = Self::connect(&self.file)?;
        let has_rowid = table_has_rowid(&conn, table);
        let id = row_name.strip_suffix(".json").unwrap_or(row_name);
        let names = column_names(&conn, table)?;
        let columns = names.join(", ");
        let (sql, selector): (String, i64) = if has_rowid {
            (
                format!(
                    "SELECT {columns} FROM \"{}\" WHERE rowid = ?1",
                    escape(table)
                ),
                id.parse()
                    .map_err(|_| Error::msg(format!("{row_name}: not a row id")))?,
            )
        } else {
            // No rowid to address by: the JSON is read back at the same offset
            // the listing gave the row, `<n>.json` being the row's position.
            let n: i64 = id
                .parse()
                .map_err(|_| Error::msg(format!("{row_name}: not a row")))?;
            (
                format!(
                    "SELECT {columns} FROM \"{}\" LIMIT 1 OFFSET ?1",
                    escape(table)
                ),
                n,
            )
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| Error::msg(format!("reading a row: {e}")))?;
        let mut rows = stmt
            .query([selector])
            .map_err(|e| Error::msg(format!("reading a row: {e}")))?;
        let row = rows
            .next()
            .map_err(|e| Error::msg(format!("reading a row: {e}")))?
            .ok_or_else(|| Error::msg(format!("{row_name}: no such row")))?;
        let mut obj = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            let value = row
                .get_ref(i)
                .map_err(|e| Error::msg(format!("a column: {e}")))?;
            obj.insert(name.clone(), json_of(value));
        }
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
            .map_err(|e| Error::msg(format!("serialising a row: {e}")))?;
        Ok(text.into_bytes())
    }
}

/// A table's column names, in declaration order.
fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let stmt = conn
        .prepare(&format!("SELECT * FROM \"{}\" LIMIT 0", escape(table)))
        .map_err(|e| Error::msg(format!("{table}: not a readable table ({e})")))?;
    Ok(stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect())
}

/// The column whose value is the rowid, if the table declares one.
///
/// A single-column `INTEGER PRIMARY KEY` on a rowid table is an alias for the
/// rowid: selecting it and selecting `rowid` return the same number. The
/// listing shows the rowid as the row's name, so naming that column separately
/// would draw the same id in two columns; instead its name titles the Name
/// column. Anything else - a composite key, a text key, a `WITHOUT ROWID`
/// table - has no such alias and the row's name stays the plain "Name".
fn rowid_alias(conn: &Connection, table: &str, has_rowid: bool) -> Option<String> {
    if !has_rowid {
        return None;
    }
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{}\")", escape(table)))
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            let name: String = row.get("name")?;
            let ty: String = row.get("type")?;
            let pk: i64 = row.get("pk")?;
            Ok((name, ty, pk))
        })
        .ok()?;
    let pk_cols: Vec<(String, String)> = rows
        .filter_map(std::result::Result::ok)
        .filter(|(_, _, pk)| *pk > 0)
        .map(|(name, ty, _)| (name, ty))
        .collect();
    match pk_cols.as_slice() {
        // Exactly one key column, declared INTEGER: the rowid alias.
        [(name, ty)] if ty.to_ascii_uppercase().contains("INT") => Some(name.clone()),
        _ => None,
    }
}

/// How wide the id (Name) column is drawn: room for its header and a typical
/// id, and no more, since the id is not where a table's width should go.
fn id_width(header: &str) -> u16 {
    u16::try_from(header.chars().count())
        .unwrap_or(u16::MAX)
        .clamp(6, 16)
}

/// How wide a data column is drawn: room for its header, since that is what the
/// column is known to hold before a single row is read. Values wider than this
/// crop, the same as an over-long file name does, and narrower tables pack
/// tight rather than stretching one column across the panel.
fn data_width(header: &str) -> u16 {
    u16::try_from(header.chars().count())
        .unwrap_or(u16::MAX)
        .saturating_add(1)
        .clamp(6, 24)
}

/// Whether a table has an addressable rowid. A `WITHOUT ROWID` table does not,
/// and its rows are addressed by position instead.
fn table_has_rowid(conn: &Connection, table: &str) -> bool {
    // `rowid` selects on an ordinary table and errors on a `WITHOUT ROWID`
    // one, which is the cheapest way to tell them apart without parsing DDL.
    conn.prepare(&format!("SELECT rowid FROM \"{}\" LIMIT 0", escape(table)))
        .is_ok()
}

/// One page of a table's rows as entries.
fn read_page(
    conn: &Connection,
    table: &str,
    shown: &[String],
    has_rowid: bool,
    offset: usize,
) -> Result<Vec<Entry>> {
    let id_expr = if has_rowid { "rowid" } else { "NULL" };
    let selected = if shown.is_empty() {
        String::new()
    } else {
        let cols: Vec<String> = shown.iter().map(|c| format!("\"{}\"", escape(c))).collect();
        format!(", {}", cols.join(", "))
    };
    let sql = format!(
        "SELECT {id_expr}{selected} FROM \"{}\" LIMIT {PAGE} OFFSET {offset}",
        escape(table)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::msg(format!("reading {table}: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| Error::msg(format!("reading {table}: {e}")))?;
    let mut out = Vec::new();
    let mut n = offset;
    while let Some(row) = rows.next().map_err(|e| Error::msg(format!("a row: {e}")))? {
        // The name is the row's own id - the rowid where there is one, else
        // its position - shown plainly, without the `.json` the copy carries.
        // `Enter` reads the same row back by it, and `F5` writes it out as
        // `<database>.<table>.<id>.json`.
        let id: Option<i64> = row.get(0).ok();
        let name = match (has_rowid, id) {
            (true, Some(id)) => id.to_string(),
            _ => n.to_string(),
        };
        let mut entry = Entry::file(name);
        entry.cells = (0..shown.len())
            .map(|i| row.get_ref(i + 1).map_or(CellValue::Null, cell_of))
            .collect();
        out.push(entry);
        n = n.saturating_add(1);
    }
    Ok(out)
}

/// A cell value for the sortable columns, typed so numbers sort as numbers.
fn cell_of(value: rusqlite::types::ValueRef<'_>) -> CellValue {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => CellValue::Null,
        ValueRef::Integer(n) => CellValue::Int(n),
        ValueRef::Real(x) => CellValue::Real(x),
        ValueRef::Text(bytes) => CellValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        // A blob is not a cell to sort by; its size is the honest summary.
        ValueRef::Blob(bytes) => CellValue::Text(format!("<{} bytes>", bytes.len())),
    }
}

/// A JSON value for a cell of a row's full record.
fn json_of(value: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    use serde_json::Value;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => Value::from(n),
        ValueRef::Real(x) => serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        // A blob has no JSON form worth guessing at; its size stands in, the
        // same summary the cell shows.
        ValueRef::Blob(bytes) => Value::String(format!("<{} bytes>", bytes.len())),
    }
}

/// Double any `"` in an identifier, so a table or column named with one cannot
/// break out of the quotes it is wrapped in. There are no parameters for
/// identifiers in SQLite; quoting is the whole of the defence.
fn escape(identifier: &str) -> String {
    identifier.replace('"', "\"\"")
}

impl Vfs for SqliteFs {
    fn kind(&self) -> BackendKind {
        BackendKind::Sqlite
    }

    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let this = self.clone();
        // No `..` row here: the read path prepends it for every backend alike,
        // and sending one as well is how a listing ends up with two of them.
        let location = Self::locate(path);
        tokio::task::spawn_blocking(move || match location {
            Location::Tables => match this.list_tables() {
                Ok(rows) => {
                    for row in rows {
                        if tx.blocking_send(Ok(row)).is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                }
            },
            Location::Rows(table) => this.stream_rows(table, &tx),
        });
        rx
    }

    fn column_plan(&self, path: &VfsPath) -> Option<ColumnPlan> {
        match Self::locate(path) {
            // The table list is directories; the name is all it has.
            Location::Tables => None,
            Location::Rows(table) => self.table_columns(&table).ok().map(|(plan, _)| plan),
        }
    }

    fn view_format(&self, path: &VfsPath) -> Option<String> {
        // A row is named for its id and reads as JSON: the viewer highlights
        // and renders it as such though the name carries no `.json`.
        match Self::locate(path) {
            Location::Rows(tail) if !split_row(&tail).1.is_empty() => Some("json".to_string()),
            _ => None,
        }
    }

    fn copy_name(&self, path: &VfsPath) -> Option<String> {
        // `<database>.<table>.<id>.json`: the row sitting in another panel says
        // where it came from, and copies out as the JSON it is - the `.json`
        // the plain listing name does not carry.
        let Location::Rows(tail) = Self::locate(path) else {
            return None;
        };
        let (table, row) = split_row(&tail);
        if row.is_empty() {
            return None;
        }
        let db = self.file.file_name()?.to_string_lossy();
        Some(format!("{db}.{table}.{row}.json"))
    }

    fn describe(&self, path: &VfsPath) -> Option<String> {
        let db = self
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(match Self::locate(path) {
            Location::Tables => format!("[sqlite: {db}]"),
            Location::Rows(table) => format!("[{table}: {db}]"),
        })
    }

    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        match Self::locate(path) {
            Location::Tables => Ok(Entry::dir("sqlite")),
            Location::Rows(tail) => {
                // `table` is a directory; `table/<id>` is a row within it. The
                // `/` is the whole of the distinction, now that the id stands
                // on its own without an extension to tell them apart.
                let (tbl, row) = split_row(&tail);
                if !row.is_empty() {
                    let bytes = self.row_json(tbl, row)?;
                    let mut entry = Entry::file(row.to_string());
                    entry.size = bytes.len() as u64;
                    Ok(entry)
                } else {
                    Ok(Entry::dir(tail))
                }
            }
        }
    }

    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let Location::Rows(tail) = Self::locate(path) else {
            return Err(Error::msg(format!("{path} is not a row")));
        };
        let (table, row) = split_row(&tail);
        let bytes = self.row_json(table, row)?;
        Ok(Box::new(std::io::Cursor::new(bytes)))
    }

    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        let Location::Rows(tail) = Self::locate(path) else {
            return Err(Error::msg(format!("{path} is not a row")));
        };
        let (table, row) = split_row(&tail);
        let bytes = self.row_json(table, row)?;
        Ok(Box::new(std::io::Cursor::new(bytes)))
    }

    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        let _ = path;
        Err(Error::msg(
            "a database is read-only here; nothing is written back",
        ))
    }

    fn create_dir(&self, _path: &VfsPath) -> Result<()> {
        Err(Error::msg("a database is read-only here"))
    }

    fn remove(&self, _path: &VfsPath) -> Result<()> {
        Err(Error::msg("a database is read-only here"))
    }

    fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
        Err(Error::msg("a database is read-only here"))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SQLITE
    }
}

use crate::vfs::Capabilities;

/// Split a `#sqlite` tail of the form `table/<row>.json` into the table and
/// the row name. A tail with no `/` is a table with an empty row.
fn split_row(tail: &str) -> (&str, &str) {
    tail.rsplit_once('/').unwrap_or((tail, ""))
}

#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
