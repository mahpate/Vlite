#![cfg(feature = "tools")]
mod cli_support;
use cli_support::{Fixture, success};
use std::io::Write;
use std::process::{Command, Stdio};
#[test]
fn script_crud_filters_ann_browse_and_maintenance() {
    let f = Fixture::new();
    let db = f.path("db.vlite");
    let script = f.path("script.txt");
    let backup = f.path("backup.vlite");
    std::fs::write(&script,format!(r#"# shell fixture
help
put {{"id":7,"vector":[1,2],"payload":"hello world","metadata":{{"tag":{{"type":"string","value":"yes"}}}}}}
put {{"id":8,"vector":[9,2]}}
get 7
header
format
collections
stats
use default
browse 1
search {{"vector":[1,2],"k":1,"filters":[{{"op":"eq","field":"tag","value":{{"type":"string","value":"yes"}}}}]}}
rebuild
search {{"vector":[1,2],"k":1,"ann":true}}
delete 8
verify
backup {}
quit
"#,backup.display())).unwrap();
    let out = success(
        Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
            .args([
                "--db",
                db.to_str().unwrap(),
                "--create",
                "2",
                "--script",
                script.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    assert!(out.contains("hello world"));
    assert!(out.contains("\"verified\":true"));
    assert!(out.contains("\"id\":7"));
    assert!(backup.exists());
    let db = vlite::Database::open(db).unwrap();
    assert_eq!(db.len(), 1);
    assert!(db.get(8).unwrap().is_none());
}
#[test]
fn readonly_stdin_is_byte_preserving_and_blocks_writes() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let before = std::fs::read(&db).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
        .args(["--db", db.to_str().unwrap(), "--read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"get 1\nsearch {\"vector\":[1,1],\"ann\":true,\"k\":2}\nverify\ndelete 1\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("read-only"));
    assert_eq!(before, std::fs::read(db).unwrap());
}
#[test]
fn script_errors_are_nonzero() {
    let f = Fixture::new();
    let db = f.path("db");
    let script = f.path("script");
    cli_support::database(&db);
    std::fs::write(&script, "not-a-command\n").unwrap();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
            .args([
                "--db",
                db.to_str().unwrap(),
                "--script",
                script.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
}
