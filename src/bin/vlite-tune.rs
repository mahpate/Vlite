#[path = "tool_support/quality_tools.rs"]
mod quality_tools;
mod tool_support;
use serde_json::json;
use std::process::ExitCode;
use tool_support::{Result, main_result};
use vlite::{Database, HnswConfig};
const HELP: &str = "vlite-tune --db PATH [--collection NAME] [--m 8,16,32] [--ef-construction 64,128] [--ef-search 20,50,100] [--target-recall 0.95] [--k 10] [--samples 8] [--memory-weight 1] [--json]\nSweeps deterministic L2 HNSW on stored-vector queries; never changes database bytes.\nCheapest = measured mean query microseconds + memory-weight * component MiB.\nMemory excludes allocator/node overhead; latency is a local estimate, not a guarantee.\nNo recommendation (nonzero exit) if no candidate reaches target.\n";
fn list(
    args: &std::collections::BTreeMap<String, String>,
    key: &str,
    default: &str,
) -> Result<Vec<usize>> {
    args.get(key)
        .map(String::as_str)
        .unwrap_or(default)
        .split(',')
        .map(|s| s.parse().map_err(Into::into))
        .collect()
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
            "--target-recall",
            "--k",
            "--samples",
            "--memory-weight",
            "--json",
        ],
    )?;
    if a.contains_key("--help") {
        print!("{HELP}");
        return Ok(());
    }
    let mut db = Database::open(tool_support::required(&a, "--db")?)?;
    let collection = a
        .get("--collection")
        .map(String::as_str)
        .unwrap_or("default");
    let k = a.get("--k").map(|v| v.parse()).transpose()?.unwrap_or(10);
    let samples = a
        .get("--samples")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(8);
    quality_tools::validate(samples, k)?;
    let target: f64 = a
        .get("--target-recall")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(0.95);
    let weight: f64 = a
        .get("--memory-weight")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(1.0);
    if !target.is_finite() || !(0.0..=1.0).contains(&target) || !weight.is_finite() || weight < 0.0
    {
        return Err("invalid target-recall or memory-weight".into());
    }
    let queries = quality_tools::sample(&db, collection, samples)?;
    let ms = list(&a, "--m", "8,16,32")?;
    let constructions = list(&a, "--ef-construction", "64,128")?;
    let searches = list(&a, "--ef-search", "20,50,100")?;
    if ms
        .len()
        .saturating_mul(constructions.len())
        .saturating_mul(searches.len())
        > 256
    {
        return Err("sweep limited to 256 configurations".into());
    }
    let (mut results, mut best, mut cost) = (Vec::new(), None, f64::INFINITY);
    for m in ms {
        for &ef_construction in &constructions {
            for &ef in &searches {
                let config = HnswConfig {
                    m,
                    ef_construction,
                    ..Default::default()
                };
                let evaluation =
                    quality_tools::evaluate(&mut db, collection, &queries, config, ef, k)?;
                let candidate =
                    evaluation.latency + weight * evaluation.memory as f64 / (1024.0 * 1024.0);
                let mut report = evaluation.report;
                report["cost"] = json!(candidate);
                if evaluation.recall >= target && candidate < cost {
                    best = Some(report.clone());
                    cost = candidate;
                }
                results.push(report);
            }
        }
    }
    let found = best.is_some();
    quality_tools::print_report(
        &json!({"target_recall":target,"metric":"squared_l2","sample_source":"stored vectors; seed 0","cost_formula":"mean_query_us + memory_weight * component_MiB","memory_weight":weight,"recommended":best,"candidates":results}),
        a.contains_key("--json"),
    );
    db.close()?;
    if !found {
        return Err("no configuration meets target recall".into());
    }
    Ok(())
}
fn main() -> ExitCode {
    main_result(run())
}
