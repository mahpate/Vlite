mod tool_support;
use serde::Deserialize;
use serde_json::json;
use std::{
    fs::File,
    io::{self, BufRead, IsTerminal, Write},
    process::ExitCode,
};
use tool_support::{
    Result,
    line::{Line, read_bounded_line},
    main_result,
};
use vlite::{Condition, Database, DistanceMetric, HnswConfig, Metadata};
/// Longest command line, excluding its terminator; a longer line fails like any
/// other command error. The largest command is a put of a record with 65,536
/// dimensions, a 1 MiB payload and 64 KiB of metadata, about 8.7 MB as text (a
/// search or a KV request is smaller), so the limit is nearly twice that.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const HELP: &str = r#"vlite-shell --db PATH [--read-only] [--create DIMENSIONS] [--script FILE]
Without --script reads stdin; prompts only on a terminal. One command per line.
Commands: help | header | format | collections | stats | use NAME | browse [LIMIT] [AFTER_ID]
get ID | delete ID | put {"id":1,"vector":[1,2],"payload":"text","metadata":{}}
search {"vector":[1,2],"k":10,"ann":false,"ef_search":100,"metric":"squared_l2","filters":[]}
kv-get REQUEST | kv-put REQUEST | kv-delete REQUEST | kv-list REQUEST
kv-get-many REQUEST | kv-batch REQUEST
KV REQUEST is one JSON object with required collection and namespace; see docs/TOOLS.md.
kv-get-many takes collection and keys, kv-batch takes collection and operations; each key or operation names its own namespace.
Metadata uses typed values, e.g. {"tag":{"type":"string","value":"x"}}.
Filters use engine JSON: [{"op":"eq","field":"tag","value":{"type":"string","value":"x"}}].
verify | backup DEST | compact DEST | upgrade DEST | rebuild | quit
--create creates a new graph-enabled v5 database; refuses existing paths. Read-only forbids all writes
and copy-out maintenance, but allows in-memory ANN. Errors fail scripts immediately.
"#;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    id: u64,
    vector: Vec<f32>,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    metadata: Metadata,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Search {
    vector: Vec<f32>,
    k: usize,
    ann: bool,
    ef_search: usize,
    metric: DistanceMetric,
    filters: Vec<Condition>,
}
impl Default for Search {
    fn default() -> Self {
        Self {
            vector: vec![],
            k: 10,
            ann: false,
            ef_search: 100,
            metric: DistanceMetric::SquaredL2,
            filters: vec![],
        }
    }
}
// Read-only sessions hold the shared lock, so other readers can open the file.
fn open(path: &str, readonly: bool) -> Result<Database> {
    Ok(if readonly {
        Database::open_read_only(path)?
    } else {
        Database::open(path)?
    })
}
fn run() -> Result<()> {
    let args = tool_support::args()?;
    tool_support::check_args(
        &args,
        &["--help", "--db", "--read-only", "--create", "--script"],
    )?;
    if args.contains_key("--help") {
        print!("{HELP}");
        return Ok(());
    }
    let path = tool_support::required(&args, "--db")?;
    let readonly = args.contains_key("--read-only");
    if readonly && args.contains_key("--create") {
        return Err("--read-only conflicts with --create".into());
    }
    let mut db = Some(if let Some(d) = args.get("--create") {
        Database::create(path, d.parse()?)?
    } else {
        open(path, readonly)?
    });
    let interactive = !args.contains_key("--script") && io::stdin().is_terminal();
    let mut input: Box<dyn BufRead> = if let Some(p) = args.get("--script") {
        Box::new(io::BufReader::new(File::open(p)?))
    } else {
        Box::new(io::BufReader::new(io::stdin()))
    };
    session(
        &mut input,
        interactive,
        MAX_LINE_BYTES,
        readonly,
        path,
        &mut db,
    )?;
    if let Some(db) = db {
        db.close()?;
    }
    Ok(())
}
// Runs the commands in `input` until `quit` or its end. An error stops the run, except in a
// terminal session, which prints it and goes on; a line of more than `cap` bytes is such an error.
fn session(
    input: &mut impl BufRead,
    interactive: bool,
    cap: usize,
    readonly: bool,
    path: &str,
    db: &mut Option<Database>,
) -> Result<()> {
    let mut collection = "default".to_string();
    loop {
        if interactive {
            print!("vlite[{collection}]> ");
            io::stdout().flush()?;
        }
        let line = match read_bounded_line(input, cap)? {
            Line::Text(line) => line,
            Line::TooLong => {
                report(
                    interactive,
                    format!("invalid_input: line exceeds {cap} bytes").into(),
                )?;
                continue;
            }
            Line::Eof => break,
        };
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        if matches!(command, "quit" | "exit") {
            break;
        }
        let result = (|| -> Result<()> {
            if readonly && matches!(command, "put" | "delete" | "backup" | "compact" | "upgrade") {
                return Err("read-only: command forbidden".into());
            }
            if matches!(command, "verify" | "backup" | "compact" | "upgrade") {
                drop(db.take());
                let maintenance = (|| -> Result<()> {
                    match command {
                        "verify" => {
                            let r = vlite::maintenance::verify(path)?;
                            println!(
                                "{}",
                                json!({"verified":true,"bytes":r.bytes,"crc32":r.crc32})
                            );
                        }
                        "backup" => {
                            vlite::maintenance::backup(path, rest)?;
                            println!("backup complete");
                        }
                        "compact" => {
                            vlite::maintenance::compact(path, rest)?;
                            println!("compact complete");
                        }
                        _ => {
                            vlite::maintenance::upgrade(path, rest)?;
                            println!("upgrade complete");
                        }
                    }
                    Ok(())
                })();
                *db = Some(open(path, readonly)?);
                return maintenance;
            }
            let db = db.as_mut().ok_or("database unavailable")?;
            if let Some(tool) = tool_support::kv_mcp::shell_tool(command) {
                let result = tool_support::kv_mcp::run(db, readonly, tool, rest)?;
                println!("{result}");
                return Ok(());
            }
            match command {
                "help" => print!("{HELP}"),
                "header" | "format" | "stats" => println!(
                    "{}",
                    json!({"format_version":db.format_version(),"lineage":db.lineage(),"dimensions":db.collection(&collection)?.dimensions(),"records":db.collection(&collection)?.len(),"file_bytes":std::fs::metadata(path)?.len()})
                ),
                "collections" => {
                    for c in db.collections()? {
                        println!(
                            "{}",
                            json!({"name":c.name,"dimensions":c.dimensions,"records":c.live_records})
                        );
                    }
                }
                "use" => {
                    db.collection(rest)?;
                    collection = rest.into();
                }
                "get" => {
                    let id = rest.parse()?;
                    let c = db.collection(&collection)?;
                    if let Some(r) = c.get(id)? {
                        println!(
                            "{}",
                            json!({"id":id,"vector":r.vector,"payload":String::from_utf8_lossy(&r.payload),"metadata":r.metadata})
                        );
                    } else {
                        println!("null");
                    }
                }
                "put" => {
                    let p: Put = serde_json::from_str(rest)?;
                    db.collection_mut(&collection)?.put_with_metadata(
                        p.id,
                        &p.vector,
                        p.payload.as_bytes(),
                        &p.metadata,
                    )?;
                    println!("ok");
                }
                "delete" => println!("{}", db.collection_mut(&collection)?.delete(rest.parse()?)?),
                "browse" => {
                    let mut values = rest.split_whitespace();
                    let limit = values.next().map(str::parse).transpose()?.unwrap_or(20);
                    let after = values.next().map(str::parse).transpose()?;
                    if values.next().is_some() {
                        return Err("browse accepts LIMIT and optional AFTER_ID".into());
                    }
                    let page = db.collection(&collection)?.browse(after, limit)?;
                    for hit in page.hits {
                        let r = hit.record;
                        println!(
                            "{}",
                            json!({"id":hit.id,"vector":r.vector,"payload":String::from_utf8_lossy(&r.payload),"metadata":r.metadata})
                        );
                    }
                    println!("{}", json!({"next_after":page.next_after}));
                }
                "rebuild" => {
                    db.collection_mut(&collection)?
                        .build_hnsw_with_metric(HnswConfig::default(), DistanceMetric::SquaredL2)?;
                    println!("rebuilt");
                }
                "search" => {
                    let s: Search = serde_json::from_str(rest)?;
                    let hits = if s.ann {
                        db.collection_mut(&collection)?
                            .search_approximate_filtered_with_metric(
                                &s.vector,
                                s.k,
                                s.ef_search,
                                HnswConfig::default(),
                                &s.filters,
                                s.metric,
                            )?
                    } else {
                        db.collection(&collection)?
                            .search_filtered_with_metric(&s.vector, s.k, &s.filters, s.metric)?
                    };
                    println!(
                        "{}",
                        serde_json::Value::Array(
                            hits.iter()
                                .map(|h| json!({"id":h.id,"distance":h.distance}))
                                .collect()
                        )
                    );
                }
                _ => return Err(format!("unknown command {command}; use help").into()),
            }
            Ok(())
        })();
        if let Err(e) = result {
            report(interactive, e)?;
        }
    }
    Ok(())
}
// A terminal session prints the error and goes on; a script or piped input stops.
fn report(interactive: bool, error: Box<dyn std::error::Error>) -> Result<()> {
    if interactive {
        eprintln!("error: {error}");
        Ok(())
    } else {
        Err(error)
    }
}
fn main() -> ExitCode {
    main_result(run())
}
#[cfg(test)]
mod tests {
    use super::{report, session};
    use std::io::Cursor;
    use vlite::Database;

    #[test]
    fn a_terminal_session_goes_on_after_an_error_and_a_script_stops() {
        assert!(report(true, "failed".into()).is_ok());
        assert_eq!(
            report(false, "failed".into()).unwrap_err().to_string(),
            "failed"
        );
    }

    // Runs `put 1`, a comment line of 65 bytes under a cap of 64, and `put 2`. Returns
    // the error that ended the session, if any, and whether each put was stored.
    fn put_around_an_overlong_line(interactive: bool) -> (Option<String>, [bool; 2]) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lines.vlite");
        let path = path.to_str().unwrap();
        let mut db = Some(Database::create(path, 2).unwrap());
        let script = format!(
            "put {{\"id\":1,\"vector\":[1,2]}}\n#{}\nput {{\"id\":2,\"vector\":[3,4]}}\n",
            "x".repeat(64)
        );
        let result = session(
            &mut Cursor::new(script),
            interactive,
            64,
            false,
            path,
            &mut db,
        );
        let db = db.unwrap();
        (
            result.err().map(|error| error.to_string()),
            [1, 2].map(|id| db.get(id).unwrap().is_some()),
        )
    }

    #[test]
    fn an_overlong_line_is_skipped_in_a_terminal_session_and_stops_a_script() {
        assert_eq!(put_around_an_overlong_line(true), (None, [true, true]));
        assert_eq!(
            put_around_an_overlong_line(false),
            (
                Some("invalid_input: line exceeds 64 bytes".to_string()),
                [true, false]
            )
        );
    }
}
