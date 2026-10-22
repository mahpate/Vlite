#![allow(dead_code)]
use std::{collections::BTreeMap, error::Error, process::ExitCode};
use vlite::{CollectionRef, Record};
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub fn main_result(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
pub fn args() -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        if !key.starts_with("--") {
            return Err(format!("expected --option, got {key}").into());
        }
        let value = if matches!(key.as_str(), "--help" | "--json" | "--read-only") {
            "true".into()
        } else {
            args.next()
                .ok_or_else(|| format!("missing value for {key}"))?
        };
        if out.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate option {key}").into());
        }
    }
    Ok(out)
}
pub fn check_args(args: &BTreeMap<String, String>, allowed: &[&str]) -> Result<()> {
    for key in args.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown option {key}").into());
        }
    }
    Ok(())
}
pub fn required<'a>(args: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    args.get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {key}; use --help").into())
}
pub fn records(collection: CollectionRef<'_>) -> Result<Vec<(u64, Record)>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = collection.browse(after, vlite::records::MAX_BROWSE_PAGE)?;
        out.extend(page.hits.into_iter().map(|hit| (hit.id, hit.record)));
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    Ok(out)
}
