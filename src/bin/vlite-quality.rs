#[path = "tool_support/quality_tools.rs"]
mod quality_tools;
mod tool_support;
use serde_json::json;
use std::process::ExitCode;
use tool_support::{Result, main_result};
use vlite::{Database, HnswConfig};
const HELP: &str = "vlite-quality --db PATH [--collection NAME] [--m 16] [--ef-construction 100] [--ef-search 100] [--k 10] [--samples 8] [--json]\n [--other-db PATH] [--other-m N] [--other-ef-construction N] [--other-ef-search N]\nANN vs exact: recall@k, Jaccard overlap, rank correlation on shared IDs, latency, memory.\nOptional comparison uses the SAME source queries for both files/configurations.\nDatabase comparison assumes corresponding IDs identify the same records. L2 only.\nFiles are opened exclusively; indexes are in memory, datafiles are never changed.\n";
fn number(
    a: &std::collections::BTreeMap<String, String>,
    key: &str,
    default: usize,
) -> Result<usize> {
    Ok(a.get(key)
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(default))
}
fn run() -> Result<()> {
    let a = tool_support::args()?;
    tool_support::check_args(
        &a,
        &[
            "--help",
            "--db",
            "--collection",
            "--m",
            "--ef-construction",
            "--ef-search",
            "--k",
            "--samples",
            "--json",
            "--other-db",
            "--other-m",
            "--other-ef-construction",
            "--other-ef-search",
        ],
    )?;
    if a.contains_key("--help") {
        print!("{HELP}");
        return Ok(());
    }
    let path = tool_support::required(&a, "--db")?;
    let mut db = Database::open(path)?;
    let collection = a
        .get("--collection")
        .map(String::as_str)
        .unwrap_or("default");
    let k = number(&a, "--k", 10)?;
    let samples = number(&a, "--samples", 8)?;
    quality_tools::validate(samples, k)?;
    let config = HnswConfig {
        m: number(&a, "--m", 16)?,
        ef_construction: number(&a, "--ef-construction", 100)?,
        ..Default::default()
    };
    let ef = number(&a, "--ef-search", 100)?;
    let queries = quality_tools::sample(&db, collection, samples)?;
    let first = quality_tools::evaluate(&mut db, collection, &queries, config, ef, k)?;
    let mut report = json!({"metric":"squared_l2","sample_source":"stored vectors; seed 0","primary":first.report});
    if a.keys().any(|k| k.starts_with("--other-")) {
        let other_config = HnswConfig {
            m: number(&a, "--other-m", config.m)?,
            ef_construction: number(&a, "--other-ef-construction", config.ef_construction)?,
            ..config
        };
        let other_ef = number(&a, "--other-ef-search", ef)?;
        // Release the first handle before opening the second, including aliases of the same path.
        db.close()?;
        let mut other = Database::open(a.get("--other-db").map(String::as_str).unwrap_or(path))?;
        let second =
            quality_tools::evaluate(&mut other, collection, &queries, other_config, other_ef, k)?;
        let (mut recall, mut overlap, mut ranks) = (0.0, 0.0, Vec::new());
        for (a, b) in first.hits.iter().zip(&second.hits) {
            let (r, o, s) = quality_tools::overlap(a, b);
            recall += r;
            overlap += o;
            if let Some(s) = s {
                ranks.push(s);
            }
        }
        report["secondary"] = second.report;
        report["comparison"] = json!({"overlap_at_k":recall/queries.len() as f64,"overlap_jaccard":overlap/queries.len() as f64,"rank_correlation":if ranks.is_empty(){None}else{Some(ranks.iter().sum::<f64>()/ranks.len() as f64)}});
        other.close()?;
    } else {
        db.close()?;
    }
    quality_tools::print_report(&report, a.contains_key("--json"));
    Ok(())
}
fn main() -> ExitCode {
    main_result(run())
}
