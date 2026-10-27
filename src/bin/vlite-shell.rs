mod tool_support;
use serde::Deserialize;
use serde_json::json;
use std::{
    fs::File,
    io::{self, BufRead, IsTerminal, Write},
    process::ExitCode,
};
use tool_support::{Result, main_result};
use vlite::{Condition, Database, DistanceMetric, HnswConfig, Metadata};
const HELP: &str = r#"vlite-shell --db PATH [--read-only] [--create DIMENSIONS] [--script FILE]
Without --script reads stdin; prompts only on a terminal. One command per line.
Commands: help | header | format | collections | stats | use NAME | browse [LIMIT] [AFTER_ID]
get ID | delete ID | put {"id":1,"vector":[1,2],"payload":"text","metadata":{}}
search {"vector":[1,2],"k":10,"ann":false,"ef_search":100,"metric":"squared_l2","filters":[]}
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
        Database::open(path)?
    });
    let interactive = !args.contains_key("--script") && io::stdin().is_terminal();
    let mut input: Box<dyn BufRead> = if let Some(p) = args.get("--script") {
        Box::new(io::BufReader::new(File::open(p)?))
    } else {
        Box::new(io::BufReader::new(io::stdin()))
    };
    let mut collection = "default".to_string();
    let mut line = String::new();
    loop {
        if interactive {
            print!("vlite[{collection}]> ");
            io::stdout().flush()?;
        }
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
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
                db = Some(Database::open(path)?);
                return maintenance;
            }
            let db = db.as_mut().ok_or("database unavailable")?;
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
            if interactive {
                eprintln!("error: {e}");
            } else {
                return Err(e);
            }
        }
    }
    if let Some(db) = db {
        db.close()?;
    }
    Ok(())
}
fn main() -> ExitCode {
    main_result(run())
}
