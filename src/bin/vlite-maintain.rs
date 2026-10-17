use std::error::Error;
use std::process::ExitCode;
use vlite::maintenance::{self, Inspection};

const USAGE: &str = "usage: vlite-maintain inspect SOURCE | backup SOURCE DESTINATION | \
restore BACKUP DESTINATION | compact SOURCE DESTINATION";

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
