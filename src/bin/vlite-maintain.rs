use std::error::Error;
use std::process::ExitCode;
use vlite::maintenance::{self, Inspection};

macro_rules! usage {
    () => {
        "usage: vlite-maintain inspect SOURCE | backup SOURCE DESTINATION | \
restore BACKUP DESTINATION | compact SOURCE DESTINATION | upgrade SOURCE DESTINATION | \
verify PATH | drill BACKUP WORKDIR"
    };
}

#[cfg(not(feature = "graph"))]
const USAGE: &str = usage!();
#[cfg(feature = "graph")]
const USAGE: &str = concat!(
    usage!(),
    " | enable-graph SOURCE DESTINATION | strip-graph SOURCE DESTINATION --confirm-graph-data-loss"
);

fn print_report(report: &Inspection) {
    println!(
        "format_version={} dimensions={} live_records={} commits={} file_bytes={} valid_bytes={} incomplete_tail_bytes={}",
        report.format_version,
        report.dimensions,
        report.live_records,
        report.commits,
        report.file_bytes,
        report.valid_bytes,
        report.incomplete_tail_bytes
    );
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let action = args.first().and_then(|action| action.to_str());
    match (action, args.len()) {
        (Some("inspect"), 2) => {
            let report = maintenance::inspect(&args[1])?;
            print_report(&report);
            if report.incomplete_tail_bytes != 0 {
                return Err("incomplete tail detected; no data was modified".into());
            }
        }
        (Some("backup"), 3) => print_report(&maintenance::backup(&args[1], &args[2])?),
        (Some("restore"), 3) => print_report(&maintenance::restore(&args[1], &args[2])?),
        (Some("compact"), 3) => {
            let report = maintenance::compact(&args[1], &args[2])?;
            println!(
                "source_bytes={} source_commits={}",
                report.source.file_bytes, report.source.commits
            );
            print_report(&report.output);
        }
        (Some("upgrade"), 3) => {
            let report = maintenance::upgrade(&args[1], &args[2])?;
            println!(
                "source_format_version={} source_bytes={}",
                report.source.format_version, report.source.file_bytes
            );
            print_report(&report.output);
        }
        (Some("verify"), 2) => {
            let report = maintenance::verify(&args[1])?;
            print_report(&report.inspection);
            println!("bytes={} crc32={:08x}", report.bytes, report.crc32);
        }
        (Some("drill"), 3) => {
            let report = maintenance::drill(&args[1], &args[2])?;
            print_report(&report.backup);
            println!(
                "drill=passed collections={} probes={}",
                report.collections, report.probes
            );
        }
        #[cfg(feature = "graph")]
        (Some("enable-graph"), 3) => {
            let report = maintenance::enable_graph(&args[1], &args[2], None)?;
            println!(
                "source_format_version={} source_bytes={}",
                report.source.format_version, report.source.file_bytes
            );
            print_report(&report.output);
        }
        #[cfg(feature = "graph")]
        (Some("strip-graph"), 3 | 4) => {
            // Without the flag the library refuses (confirm_graph_data_loss).
            let confirm = match args.get(3) {
                None => false,
                Some(flag) if flag == "--confirm-graph-data-loss" => true,
                Some(_) => return Err(USAGE.into()),
            };
            let report = maintenance::strip_graph(&args[1], &args[2], None, confirm)?;
            println!(
                "removed_graphs={} removed_nodes={} removed_edges={} removed_attachments={}",
                report.removed.graphs,
                report.removed.nodes,
                report.removed.edges,
                report.removed.attachments
            );
            print_report(&report.compaction.output);
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
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
