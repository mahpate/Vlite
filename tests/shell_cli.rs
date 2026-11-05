#![cfg(feature = "tools")]
mod cli_support;
use cli_support::{Fixture, success};
use std::io::{BufRead, BufReader, Write};
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
    for help in [
        out.as_str(),
        &success(
            Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
                .arg("--help")
                .output()
                .unwrap(),
        ),
    ] {
        assert!(
            help.lines().any(|line| line
                == "kv-get REQUEST | kv-put REQUEST | kv-delete REQUEST | kv-list REQUEST")
        );
        assert!(help.lines().any(|line| line
            == "KV REQUEST is one JSON object with required collection and namespace; see docs/TOOLS.md."));
    }
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

struct Shell(std::process::Child);

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// `--read-only` only filters commands unless the file is opened read-only: the
// writer lock would keep every other reader and the server out while the shell
// idles at its prompt. As `vlite-mcp` does, it must hold a shared lock.
#[test]
fn readonly_shell_holds_a_shared_lock_so_other_readers_still_open() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let mut shell = Shell(
        Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
            .args(["--db", db.to_str().unwrap(), "--read-only"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = shell.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(shell.0.stdout.take().unwrap());
    {
        let mut ask = |command: &str| {
            writeln!(stdin, "{command}").unwrap();
            stdin.flush().unwrap();
            let mut answer = String::new();
            stdout.read_line(&mut answer).unwrap();
            assert!(
                !answer.is_empty(),
                "the shell ended before it answered {command}"
            );
        };
        // Once the shell answers it holds the file. `verify` closes the file for
        // the check and opens it again, so the second round probes that handle.
        let rounds = [
            vec![
                "header",
                "rebuild",
                r#"search {"vector":[1,1],"k":2,"ann":true}"#,
            ],
            vec!["verify", "header"],
        ];
        for commands in rounds {
            for command in commands {
                ask(command);
            }
            let reader = vlite::Database::open_read_only(&db)
                .expect("a second read-only open must succeed while the shell idles");
            assert!(
                matches!(vlite::Database::open(&db), Err(vlite::Error::Locked)),
                "a writer must still be refused"
            );
            drop(reader);
        }
    }
    drop(stdin);
    assert!(shell.0.wait().unwrap().success());
}

/// A shell kept at its prompt. Each command is answered with one line, so once
/// `ask` returns the shell holds the file (or has opened it again).
struct Session {
    shell: Shell,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Session {
    fn start(args: &[&str]) -> Self {
        let mut shell = Shell(
            Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdin = shell.0.stdin.take().unwrap();
        let stdout = BufReader::new(shell.0.stdout.take().unwrap());
        Self {
            shell,
            stdin,
            stdout,
        }
    }

    fn ask(&mut self, command: &str) -> String {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
        let mut answer = String::new();
        self.stdout.read_line(&mut answer).unwrap();
        assert!(
            !answer.is_empty(),
            "the shell ended before it answered {command}"
        );
        answer.trim().to_owned()
    }

    fn finish(mut self) -> std::process::ExitStatus {
        drop(self.stdin);
        self.shell.0.wait().unwrap()
    }
}

fn readers_are_refused(db: &std::path::Path) -> bool {
    matches!(
        vlite::Database::open_read_only(db),
        Err(vlite::Error::Locked)
    )
}

const WRITE: &str = r#"put {"id":90,"vector":[1,1],"payload":"written"}"#;

// docs/TOOLS.md: the shell takes an exclusive file lock except with
// --read-only, so an existing file opened without --create is the writer.
#[test]
fn an_unflagged_shell_opens_writable_and_holds_the_exclusive_lock() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let mut shell = Session::start(&["--db", db.to_str().unwrap()]);
    shell.ask("header");
    assert!(readers_are_refused(&db), "the writer lock must be held");
    assert_eq!(shell.ask(WRITE), "ok");
    assert!(shell.finish().success());
    let reopened = vlite::Database::open(&db).unwrap();
    assert_eq!(reopened.get(90).unwrap().unwrap().payload, b"written");
}

// `verify` closes the file and opens it again; without --read-only that
// handle is the writer too.
#[test]
fn an_unflagged_shell_stays_writable_and_exclusive_after_verify() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let mut shell = Session::start(&["--db", db.to_str().unwrap()]);
    shell.ask("verify");
    // Answered only once the file is open again.
    shell.ask("header");
    assert!(
        readers_are_refused(&db),
        "the writer lock must be held again"
    );
    assert_eq!(shell.ask(WRITE), "ok");
    assert!(shell.finish().success());
    let reopened = vlite::Database::open(&db).unwrap();
    assert_eq!(reopened.get(90).unwrap().unwrap().payload, b"written");
}

// Creating a database is a write, so --read-only cannot be combined with it.
#[test]
fn read_only_with_create_is_refused_and_creates_nothing() {
    let f = Fixture::new();
    let db = f.path("never-created");
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
        .args(["--db", db.to_str().unwrap(), "--read-only", "--create", "2"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--read-only conflicts with --create")
    );
    assert!(!db.exists(), "a refused --create must not leave a file");
}
