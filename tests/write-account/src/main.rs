//! The guarded harness for the write phase's test account (`docs/design/writes.md` §12). It checks,
//! against OneDrive itself, what the uploads assume of the service and the wiremock suites can
//! only take as given. It writes to the drive it is pointed at, so it refuses to start unless
//! every guard holds, and never sends a request its guard has not admitted:
//!
//! 1. `--graph-test-drive` is the drive both tokens reach (`GET /me/drive`) and is listed in
//!    `write_test_drive_ids` in the daemon's `config.toml` ([`harness`]);
//! 2. the drive looks like a test account: less than 1 GiB in use, fewer than 1000 items
//!    ([`harness`]);
//! 3. every write stays in `/konedrive-write-test/<run id>/`, which the run makes and puts into
//!    the recycle bin at the end; the [`guard`] asserts it before each request, at the
//!    [`proxy`] every request goes through;
//! 4. at most 64 MiB per file, 200 MiB and 500 requests per run ([`guard::Caps`]);
//! 5. the write token comes from `konedrivectl dev export-access-token --read-write`, which the
//!    daemon hands out only for a drive on the same list ([`args`]).
//!
//! How to run it: docs/design/writes.md, "Running against the test account".

mod args;
mod checks;
mod guard;
mod harness;
mod proxy;
#[cfg(test)]
mod tests;

use std::process::ExitCode;

use clap::Parser;

use crate::harness::Ended;

fn main() -> ExitCode {
    let args = args::Args::parse();
    let options = match args::options(&args) {
        Ok(options) => options,
        Err(why) => {
            println!("REFUSED: {why}");
            return ExitCode::from(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    println!("konedrive write test {} against drive {}", options.run_id, options.test_drive);
    let (caps, run_id) = (options.caps, options.run_id.clone());
    match runtime.block_on(harness::run(options)) {
        Ended::Refused(why) => {
            println!("REFUSED: {why}");
            println!("Nothing was written.");
            ExitCode::from(2)
        }
        Ended::Ran(report) => {
            println!(
                "{} requests of {}; {:.1} MiB of content sent, of {} MiB",
                report.requests,
                caps.requests,
                report.bytes as f64 / guard::MIB as f64,
                caps.per_run / guard::MIB
            );
            if let Some(why) = &report.refused {
                println!("STOPPED: the guard refused a request of run {run_id}: {why}");
                ExitCode::from(2)
            } else if report.failed() {
                println!("FAILED: see the FAIL lines above");
                ExitCode::from(1)
            } else {
                println!("Done. Look at any LOOK line by hand.");
                ExitCode::SUCCESS
            }
        }
    }
}
