use std::error::Error;
use std::io;
use std::path::Path;
use std::process::{Command, ExitCode};
use vlite::{Database, Mutation};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn require(condition: bool, message: &str) -> Result<()> {
    if !condition {
        return Err(io::Error::new(io::ErrorKind::InvalidData, message).into());
    }
    Ok(())
}

fn initialize(path: &Path) -> Result<()> {
    let mut db = Database::create(path, 3)?;
    db.write_batch(&[
        Mutation::Put {
            id: 10,
            vector: vec![1.0, 0.0, 0.0],
            payload: b"original".to_vec(),
        },
        Mutation::Put {
            id: 20,
            vector: vec![0.0, 1.0, 0.0],
            payload: b"second".to_vec(),
        },
        Mutation::Put {
            id: 30,
            vector: vec![0.0, 0.0, 1.0],
            payload: b"temporary".to_vec(),
        },
        Mutation::Put {
            id: 10,
            vector: vec![2.0, 0.0, 0.0],
            payload: b"updated".to_vec(),
        },
        Mutation::Delete { id: 30 },
    ])?;
    require(db.get(30)?.is_none(), "batch delete failed")?;
    require(!db.delete(30)?, "missing delete unexpectedly succeeded")?;
    db.close()?;
    println!("PASS init: atomically committed inserts, update and delete");
    Ok(())
}

fn verify(path: &Path) -> Result<()> {
    let db = Database::open(path)?;
    require(db.dimensions() == 3, "unexpected database dimensions")?;
    require(db.len() == 2, "expected exactly two live records")?;
    let first = db.get(10)?;
    require(
        first.is_some_and(|r| r.vector == [2.0, 0.0, 0.0] && r.payload == b"updated"),
        "updated record did not persist",
    )?;
    let second = db.get(20)?;
    require(
        second.is_some_and(|r| r.vector == [0.0, 1.0, 0.0] && r.payload == b"second"),
        "second record did not persist",
    )?;
    require(db.get(30)?.is_none(), "deleted record reappeared")?;
    let hits = db.search(&[2.0, 0.0, 0.0], 10)?;
    require(
        hits.len() == 2
            && hits[0].id == 10
            && hits[0].distance == 0.0
            && hits[1].id == 20
            && hits[1].distance == 5.0,
        "exact search returned incorrect IDs or distances",
    )?;
    db.close()?;
    println!("PASS verify: reopened records, payloads, deletion and exact search");
    Ok(())
}

fn child(action: &str, path: &Path) -> Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg(action)
        .arg(path)
        .status()?;
    require(
        status.success(),
        &format!("{action} subprocess failed: {status}"),
    )
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().unwrap_or_else(|| "run".into());
    let path = args.next().unwrap_or_else(|| "smoke.vlite".into());
    if args.next().is_some() {
        return Err("usage: vlite-smoke [run|init|verify] [database-path]".into());
    }
    let path = Path::new(&path);
    match action.to_str() {
        Some("init") => initialize(path),
        Some("verify") => verify(path),
        Some("run") => {
            if !path.try_exists()? {
                child("init", path)?;
            }
            child("verify", path)?;
            println!("PASS end-to-end: {}", path.display());
            Ok(())
        }
        _ => Err("usage: vlite-smoke [run|init|verify] [database-path]".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FAIL: {error}");
            ExitCode::FAILURE
        }
    }
}
