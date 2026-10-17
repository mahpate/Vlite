use vlite::Database;

fn main() -> vlite::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .unwrap_or_else(|| "example.vlite".into());
    let mut db = Database::create(&path, 3)?;
    db.put(1, &[1.0, 0.0, 0.0], b"first vector")?;
    db.put(2, &[0.0, 1.0, 0.0], b"second vector")?;
    println!("{:?}", db.search(&[0.9, 0.1, 0.0], 2)?);
    db.close()?;

    let db = Database::open(&path)?;
    println!("{:?}", db.get(1)?);
    db.close()
}
