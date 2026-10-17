use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Server {
    child: Child,
    address: String,
}

impl Server {
    fn start(path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
            .arg(path)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = sender.send(line);
        });
        let mut server = Self {
            child,
            address: String::new(),
        };
        let line = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("server startup timed out");
        server.address = line
            .trim()
            .strip_prefix("LISTENING http://")
            .expect("startup address")
            .to_owned();
        server
    }

    fn request(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = body.to_string();
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()
        ).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            if status == 204 {
                Value::Null
            } else {
                serde_json::from_str(body).unwrap()
            },
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

fn fails(path: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn real_http_acknowledged_data_survives_process_termination_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("network.vlite");
    let server = Server::start(&path);
    assert_eq!(server.request("GET", "/ready", Value::Null).0, 200);
    assert_eq!(
        server
            .request(
                "POST",
                "/v1/batches",
                json!({"operations":[
                    {"op":"put","id":"18446744073709551615","vector":[1,2],"payload_base64":"AP8="},
                    {"op":"put","id":"0","vector":[4,5]},
                    {"op":"delete","id":"0"}
                ]})
            )
            .0,
        204
    );
    assert!(fails(&path, &["--bind", "127.0.0.1:0"]).contains("already locked"));
    drop(server);
    let bytes = std::fs::read(&path).unwrap();
    let server = Server::start(&path);
    let (status, record) = server.request("GET", "/v1/vectors/18446744073709551615", Value::Null);
    assert_eq!(status, 200);
    assert_eq!(record["payload_base64"], "AP8=");
    assert_eq!(server.request("GET", "/v1/vectors/0", Value::Null).0, 404);
    assert_eq!(
        server
            .request("POST", "/v1/search", json!({"vector":[1,2],"k":1}))
            .1["hits"][0]["distance"],
        0.0
    );
    drop(server);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn startup_never_overwrites_repairs_or_silently_changes_dimensions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("startup.vlite");
    assert!(fails(&path, &["--bind", "127.0.0.1:0"]).contains("FAIL:"));
    assert!(!path.exists());
    assert!(fails(&path, &["--create-if-missing", "0"]).contains("dimensions"));
    assert!(
        fails(&path, &["--create-if-missing", "2", "--bind", "0.0.0.0:0"])
            .contains("--allow-remote")
    );
    assert!(!path.exists());
    vlite::Database::create(&path, 3).unwrap().close().unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "2", "--bind", "127.0.0.1:0"]
        )
        .contains("expected 2")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut db = vlite::Database::open(&path).unwrap();
    db.put(1, &[1.0, 2.0, 3.0], b"acknowledged").unwrap();
    db.close().unwrap();
    let mut truncated = std::fs::read(&path).unwrap();
    truncated.pop();
    std::fs::write(&path, &truncated).unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "3", "--bind", "127.0.0.1:0"]
        )
        .contains("incomplete trailing write")
    );
    assert_eq!(std::fs::read(&path).unwrap(), truncated);
    std::fs::write(&path, b"corrupt; never replace").unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "2", "--bind", "127.0.0.1:0"]
        )
        .contains("FAIL:")
    );
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt; never replace");
}
