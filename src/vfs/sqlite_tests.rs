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
    assert_eq!(names, vec!["1.json", "2.json", "3.json"]);

    // The plan names the table's own columns after the row name.
    let plan = fs
        .column_plan(&people)
        .expect("a table composes its columns");
    assert_eq!(plan.columns.first(), Some(&ColumnId::Name));
    assert_eq!(plan.header(ColumnId::Custom(0)), "id");
    assert_eq!(plan.header(ColumnId::Custom(1)), "firstname");

    // And each row carries its first columns' values, typed - the age is an
    // integer, so it will sort as one.
    let alice = &listed[0];
    assert_eq!(alice.cells.first(), Some(&CellValue::Int(1)));
    assert_eq!(
        alice.cells.get(1),
        Some(&CellValue::Text("alice".to_string()))
    );
    assert_eq!(alice.cells.get(3), Some(&CellValue::Int(30)));

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
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/2.json");
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
    assert_eq!(names, vec!["0.json", "1.json"]);

    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/notes/0.json");
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
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/1.json");
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
    let row = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people/2.json");
    assert_eq!(
        fs.copy_name(&row).as_deref(),
        Some("data.sqlite.people.2.json")
    );
    // The table itself and the root have no copy name of their own.
    let table = VfsPath::local(&file).with_segment(BackendKind::Sqlite, "/people");
    assert_eq!(fs.copy_name(&table), None);
    let _ = std::fs::remove_dir_all(file.parent().unwrap_or(&file));
}
