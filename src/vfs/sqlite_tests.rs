//! Against a real database, built by `rusqlite` itself so the tests need no
//! external tool. Read-only throughout; the writes here are the fixture's, on
//! its own throwaway file.

use super::*;

/// A database with two tables: `people` with a rowid and typed columns, and
/// `notes` declared `WITHOUT ROWID`, so both addressing paths are exercised.
fn fixture(tag: &str) -> Option<(std::path::PathBuf, SqliteFs)> {
    let dir = std::env::temp_dir().join(format!("hcmd-sqlite-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let file = dir.join("data.sqlite");
    let conn = Connection::open(&file).ok()?;
    conn.execute_batch(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, firstname TEXT, lastname TEXT, age INTEGER);
         INSERT INTO people (firstname, lastname, age) VALUES ('alice', 'ng', 30);
         INSERT INTO people (firstname, lastname, age) VALUES ('bob', 'ackerman', 9);
         INSERT INTO people (firstname, lastname, age) VALUES ('carol', 'diaz', 10);
         CREATE TABLE notes (key TEXT PRIMARY KEY, body TEXT) WITHOUT ROWID;
         INSERT INTO notes VALUES ('first', 'hello');
         INSERT INTO notes VALUES ('second', 'world');",
    )
    .ok()?;
    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    let fs = SqliteFs::open(base).ok()?;
    Some((file, fs))
}

/// Drain a listing to its content rows, dropping the `..`.
async fn rows(fs: &SqliteFs, path: &VfsPath) -> Vec<Entry> {
    let mut rx = fs.read_dir(path);
    let mut out = Vec::new();
    while let Some(item) = rx.recv().await {
        if let Ok(entry) = item
            && !entry.is_parent
        {
            out.push(entry);
        }
    }
    out
}

#[tokio::test]
async fn the_root_lists_the_tables_and_names_the_panel_for_the_database() {
    let Some((file, fs)) = fixture("tables") else {
        return;
    };
    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    let names: Vec<String> = rows(&fs, &base)
        .await
        .iter()
        .map(|e| e.name.clone())
        .collect();
    // Both tables, in name order, and nothing of SQLite's own.
    assert_eq!(names, vec!["notes", "people"]);
    assert!(
        rows(&fs, &base).await.iter().all(Entry::is_dir),
        "a table is a directory"
    );
    assert_eq!(fs.describe(&base).as_deref(), Some("[sqlite: data.sqlite]"));
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[tokio::test]
async fn a_table_lists_its_rows_named_for_their_id_and_carrying_sortable_cells() {
    let Some((file, fs)) = fixture("rows") else {
        return;
    };
    let people = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    let listed = rows(&fs, &people).await;
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    // A rowid table names its rows by the id, so `F5` writes `<id>.json`.
    assert_eq!(names, vec!["1", "2", "3"], "the id value, plainly");

    // The plan names the table's own columns after the row name. The id is the
    // row's name, so it titles the Name column rather than repeating as a
    // column of its own; the data columns follow.
    let plan = fs
        .column_plan(&people)
        .expect("a table composes its columns");
    assert_eq!(plan.columns.first(), Some(&ColumnId::Name));
    assert_eq!(
        plan.header(ColumnId::Name),
        "id",
        "the Name column is the id"
    );
    assert_eq!(plan.header(ColumnId::Custom(0)), "firstname");
    assert_eq!(plan.header(ColumnId::Custom(1)), "lastname");
    // The columns pack to their own widths rather than one stretching across.
    assert!(plan.pack, "a table is a packed grid");

    // And each row carries its data columns' values, typed - the age is an
    // integer, so it will sort as one - with the id no longer among them.
    let alice = &listed[0];
    assert_eq!(
        alice.cells.first(),
        Some(&CellValue::Text("alice".to_string()))
    );
    assert_eq!(alice.cells.get(2), Some(&CellValue::Int(30)));

    // The header is the table's, not the database's.
    assert_eq!(
        fs.describe(&people).as_deref(),
        Some("[people: data.sqlite]")
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn a_row_reads_as_the_pretty_json_of_its_whole_record() {
    let Some((file, fs)) = fixture("json") else {
        return;
    };
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/2");
    let mut reader = fs.open_read(&row).expect("a row opens for reading");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut reader, &mut text).expect("read the row");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    // The whole record, not just the shown columns.
    assert_eq!(value["id"], serde_json::json!(2));
    assert_eq!(value["firstname"], serde_json::json!("bob"));
    assert_eq!(value["lastname"], serde_json::json!("ackerman"));
    assert_eq!(value["age"], serde_json::json!(9));
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[tokio::test]
async fn a_without_rowid_table_addresses_its_rows_by_position() {
    let Some((file, fs)) = fixture("norowid") else {
        return;
    };
    let notes = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/notes");
    let listed = rows(&fs, &notes).await;
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    // No rowid to name them by, so the row's position stands in.
    assert_eq!(
        names,
        vec!["0", "1"],
        "the position, no rowid to name them by"
    );

    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/notes/0");
    let mut reader = fs.open_read(&row).expect("open the row");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut reader, &mut text).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["key"], serde_json::json!("first"));
    assert_eq!(value["body"], serde_json::json!("hello"));
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn writing_to_a_database_is_refused() {
    let Some((file, fs)) = fixture("readonly") else {
        return;
    };
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/1");
    assert!(fs.open_write(&row).is_err(), "no write path exists");
    assert!(fs.remove(&row).is_err(), "nothing is removed");
    assert!(!fs.capabilities().writable, "and it says so up front");
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn a_file_that_is_not_a_database_is_refused_not_panicked() {
    let dir = std::env::temp_dir().join(format!("hcmd-sqlite-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("notes.txt");
    std::fs::write(&file, b"this is plain text, not a database\n").expect("write");
    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    assert!(SqliteFs::open(base).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_column_name_with_a_quote_in_it_cannot_break_the_query() {
    // The identifier-escaping guard: a table whose column is named with a `"`.
    let dir = std::env::temp_dir().join(format!("hcmd-sqlite-quote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("q.sqlite");
    let conn = Connection::open(&file).expect("open");
    conn.execute_batch(
        r#"CREATE TABLE weird ("od""d" TEXT, n INTEGER);
           INSERT INTO weird VALUES ('x', 1);"#,
    )
    .expect("create");
    drop(conn);
    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    let fs = SqliteFs::open(base).expect("open db");
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/weird");
    // It lists rather than erroring or doing something worse.
    let listed = rows(&fs, &table).await;
    assert_eq!(
        listed.len(),
        1,
        "the one row, and the quote did not break out"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_row_copies_out_under_its_database_and_table() {
    // Your example: employee.db, table employee, row 10000 -> a file named
    // employee.db.employee.10000.json in the other panel. The listing name
    // stays 10000.json; only what it copies to carries the lineage.
    let Some((file, fs)) = fixture("copyname") else {
        return;
    };
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/2");
    assert_eq!(
        fs.copy_name(&row).as_deref(),
        Some("data.sqlite.people.2.json")
    );
    // The table itself and the root have no copy name of their own.
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    assert_eq!(fs.copy_name(&table), None);
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn the_router_forwards_a_table_s_column_plan() {
    // The panel shows Name/Ext/Date/Attr on a table instead of its columns.
    // The backend builds the plan; the probe asks the *router*. This checks
    // the router forwards it rather than answering None (the configured set).
    let Some((file, fs)) = fixture("plan-flow") else {
        return;
    };
    let people = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    assert_eq!(
        fs.column_plan(&people)
            .map(|p| p.header(ColumnId::Custom(0)).to_string()),
        Some("firstname".to_string()),
        "the backend composes it"
    );
    let router = crate::vfs::VfsRouter::new(
        crate::config::ArchiveConfig::default(),
        crate::config::RemoteConfig::default(),
    );
    assert_eq!(
        crate::vfs::Vfs::column_plan(&router, &people)
            .map(|p| p.header(ColumnId::Custom(0)).to_string()),
        Some("firstname".to_string()),
        "and the router forwards it"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn a_row_suggests_json_so_the_viewer_highlights_it() {
    // The row is named `2`, not `2.json`, so its own name tells the viewer
    // nothing. The backend suggests `json`, and the viewer renders and
    // highlights it as such.
    let Some((file, fs)) = fixture("viewhint") else {
        return;
    };
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/2");
    assert_eq!(fs.view_format(&row).as_deref(), Some("json"));
    // A table and the root suggest nothing - they are directories.
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    assert_eq!(fs.view_format(&table), None);
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[tokio::test]
async fn a_listing_has_exactly_one_parent_row() {
    // The read path prepends `..`; the backend must not send its own, or the
    // listing shows two. The `rows` helper drops parents, so this counts them
    // straight off the receiver instead.
    let Some((file, fs)) = fixture("one-parent") else {
        return;
    };
    for tail in ["/", "/people"] {
        let path = VfsPath::local(&file).with_segment(BackendKind::Sqlite, tail);
        let mut rx = fs.read_dir(&path);
        let mut parents = 0;
        while let Some(item) = rx.recv().await {
            if item.map(|e| e.is_parent).unwrap_or(false) {
                parents += 1;
            }
        }
        // The backend sends none; the read path adds the one.
        assert_eq!(
            parents, 0,
            "the backend sends no `..` of its own for {tail}"
        );
    }
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn an_integer_primary_key_titles_the_name_column_and_narrows_it() {
    // `people.id` is an INTEGER PRIMARY KEY, an alias for the rowid the listing
    // already shows as the row's name. So it names the Name column and hands
    // its width to a data column, rather than drawing the same id twice.
    let Some((file, fs)) = fixture("idname") else {
        return;
    };
    let people = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    let plan = fs.column_plan(&people).expect("a plan");
    assert_eq!(plan.header(ColumnId::Name), "id");
    assert!(
        plan.name.is_some(),
        "the Name column carries the id's width and header"
    );
    assert!(plan.pack, "the columns pack rather than one stretching");
    assert!(
        !plan.custom.iter().any(|c| c.header == "id"),
        "the id is not also a data column"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn a_table_without_an_integer_key_keeps_the_plain_name_column() {
    // `notes` is `WITHOUT ROWID` with a text key, so its rows are named by
    // position and no column aliases that. The first column stays "Name", and
    // `key` is a data column of its own.
    let Some((file, fs)) = fixture("plainname") else {
        return;
    };
    let notes = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/notes");
    let plan = fs.column_plan(&notes).expect("a plan");
    assert_eq!(plan.header(ColumnId::Name), "Name");
    assert!(plan.pack, "a table is a packed grid");
    assert_eq!(plan.header(ColumnId::Custom(0)), "key");
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}

#[test]
fn a_column_widens_to_fit_its_values_not_only_its_header() {
    // `note` has a short header but long values; `code` has a long header but
    // short values. Each column is drawn for what it actually holds, and a
    // very long value is capped rather than running across the panel.
    let dir = std::env::temp_dir().join(format!("hcmd-sqlite-widths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("w.sqlite");
    let conn = Connection::open(&file).expect("open");
    // `note` holds a 20-char value; `code` a 1-char one; `huge` a value far
    // past the cap.
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT, code TEXT, huge TEXT);
         INSERT INTO t (note, code, huge) VALUES \
           ('a medium-length note', 'X', \
            'this value is far longer than any column should ever be drawn at all');",
    )
    .expect("seed");
    drop(conn);
    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    let fs = SqliteFs::open(base).expect("open db");
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/t");
    let plan = fs.column_plan(&table).expect("a plan");

    // 20-char value + 4 margin = 24, driven by the value, not the 4-char header.
    let note = plan.custom(ColumnId::Custom(0)).expect("note column");
    assert_eq!(note.header, "note");
    assert_eq!(note.min_chars, 24, "the column fits its longest value");

    // A one-character value under a 4-char header stays at the floor.
    let code = plan.custom(ColumnId::Custom(1)).expect("code column");
    assert_eq!(code.header, "code");
    assert_eq!(code.min_chars, 8, "a short column stays tight");

    // A value far past the cap is capped, not drawn full width.
    let huge = plan.custom(ColumnId::Custom(2)).expect("huge column");
    assert_eq!(huge.min_chars, 40, "an over-long value is capped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_table_larger_than_a_page_streams_every_row_once_and_in_order() {
    // Keyset paging must not skip or repeat a row at a page boundary, and its
    // `WHERE rowid > last` walk must step across gaps in the rowids. 1200 rows
    // span three 512-row pages; deleting a scattered few - including two on a
    // page boundary - leaves the gaps the walk has to hop.
    let dir = std::env::temp_dir().join(format!("hcmd-sqlite-pages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("big.sqlite");
    let conn = Connection::open(&file).expect("open");
    conn.execute_batch("CREATE TABLE big (id INTEGER PRIMARY KEY, v TEXT);")
        .expect("create");
    {
        let tx = conn.unchecked_transaction().expect("txn");
        let mut stmt = tx
            .prepare("INSERT INTO big (id, v) VALUES (?1, ?2)")
            .expect("prepare");
        for i in 1..=1200i64 {
            stmt.execute(rusqlite::params![i, format!("row-{i}")])
                .expect("insert");
        }
        drop(stmt);
        tx.commit().expect("commit");
    }
    let gaps = [1i64, 512, 513, 600, 1024, 1200];
    conn.execute(
        "DELETE FROM big WHERE id IN (1, 512, 513, 600, 1024, 1200)",
        [],
    )
    .expect("delete");
    drop(conn);

    let base = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/");
    let fs = SqliteFs::open(base).expect("open db");
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/big");
    let listed = rows(&fs, &table).await;
    let got: Vec<i64> = listed
        .iter()
        .map(|e| e.name.parse::<i64>().expect("a numeric id"))
        .collect();
    let want: Vec<i64> = (1..=1200).filter(|i| !gaps.contains(i)).collect();
    assert_eq!(got, want, "every surviving row, once, ascending");
    let _ = std::fs::remove_dir_all(&dir);
}
