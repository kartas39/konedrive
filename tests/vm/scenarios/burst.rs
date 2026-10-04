use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_proto::SOCKET_PATH;

use crate::harness::{Checks, Ctx, THROTTLE_SETTLE, count_in_log, occurrences_in_log, spawn_helper};

#[derive(Debug)]
pub(crate) struct BurstReport {
    pub(crate) answered: usize,
    pub(crate) ok: usize,
    pub(crate) wrong: usize,
    pub(crate) errors: BTreeMap<i32, usize>,
    failed_indices: Vec<usize>,
    /// How long each opener waited, in milliseconds, whatever it got.
    waits_ms: Vec<u64>,
    elapsed: Duration,
    peak_threads: u64,
    peak_rss_kb: u64,
}

impl std::fmt::Display for BurstReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let errs: Vec<String> =
            self.errors.iter().map(|(e, n)| format!("errno {e} x{n}")).collect();
        write!(
            f,
            "answered {}, filled {}, read-wrong {}, [{}] in {:?}; helper peaked at {} threads and \
             {} KiB RSS",
            self.answered,
            self.ok,
            self.wrong,
            errs.join(", "),
            self.elapsed,
            self.peak_threads,
            self.peak_rss_kb
        )?;
        if !self.waits_ms.is_empty() {
            let mut waits = self.waits_ms.clone();
            waits.sort_unstable();
            write!(
                f,
                "; waits: median {} ms, p95 {} ms, max {} ms",
                waits[waits.len() / 2],
                waits[waits.len() * 95 / 100],
                waits[waits.len() - 1]
            )?;
        }
        Ok(())
    }
}

pub(crate) fn run_burst(ctx: &Ctx, dir: &Path, count: usize, expect: u8) -> Result<BurstReport, String> {
    let helper_pid = ctx.helper_pid();
    let sampling = Arc::new(AtomicBool::new(true));
    let peak_threads = Arc::new(AtomicU64::new(0));
    let peak_rss = Arc::new(AtomicU64::new(0));
    let sampler = {
        let sampling = Arc::clone(&sampling);
        let peak_threads = Arc::clone(&peak_threads);
        let peak_rss = Arc::clone(&peak_rss);
        std::thread::spawn(move || {
            while sampling.load(Ordering::SeqCst) {
                if let Some((threads, rss)) = proc_status(helper_pid) {
                    peak_threads.fetch_max(threads, Ordering::SeqCst);
                    peak_rss.fetch_max(rss, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };

    let started = Instant::now();
    let output = Command::new(&ctx.exe)
        .arg("--burst")
        .arg(dir)
        .arg(count.to_string())
        .arg(expect.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run the burst: {e}"))?;
    let elapsed = started.elapsed();
    sampling.store(false, Ordering::SeqCst);
    let _ = sampler.join();

    let said = String::from_utf8_lossy(&output.stdout);
    let mut report = BurstReport {
        answered: 0,
        ok: 0,
        wrong: 0,
        errors: BTreeMap::new(),
        failed_indices: Vec::new(),
        waits_ms: Vec::new(),
        elapsed,
        peak_threads: peak_threads.load(Ordering::SeqCst),
        peak_rss_kb: peak_rss.load(Ordering::SeqCst),
    };
    for line in said.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.first().copied() {
            Some("FAILED") => {
                if let (Some(i), Some(errno)) = (fields.get(1), fields.get(2)) {
                    if let (Ok(i), Ok(errno)) = (i.parse::<usize>(), errno.parse::<i32>()) {
                        report.failed_indices.push(i);
                        *report.errors.entry(errno).or_default() += 1;
                    }
                }
                if let Some(Ok(ms)) = fields.get(3).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("WRONG") => {
                if let Some(Ok(i)) = fields.get(1).map(|s| s.parse::<usize>()) {
                    report.failed_indices.push(i);
                }
                if let Some(Ok(ms)) = fields.get(2).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("TOOK") => {
                if let Some(Ok(ms)) = fields.get(2).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("BURST") => {
                for field in &fields[1..] {
                    if let Some(v) = field.strip_prefix("answered=") {
                        report.answered = v.parse().unwrap_or(0);
                    } else if let Some(v) = field.strip_prefix("ok=") {
                        report.ok = v.parse().unwrap_or(0);
                    } else if let Some(v) = field.strip_prefix("wrong=") {
                        report.wrong = v.parse().unwrap_or(0);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(report)
}

fn proc_status(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut threads = 0;
    let mut rss = 0;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Threads:") {
            threads = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    Some((threads, rss))
}


/// The two waiter caps, measured end to end instead of asserted on a counter.
///
/// `Daemons::wait_for` is the one place a worker sleeps for tens of seconds, so
/// it is the one lever an unprivileged caller has on the pool.
/// [`MAX_DAEMON_WAITERS`] (8, per uid) and `GLOBAL_MAX_DAEMON_WAITERS` (32,
/// machine-wide) were both chosen rather than measured. With the daemon gone
/// but the root still registered, every open of a placeholder reaches
/// `Daemons::wait_for`, so the shape of the answer says exactly which cap binds:
/// an open that waited the full `DAEMON_WAIT` held a slot; one that came back
/// at once was refused by a cap.
pub(crate) fn waiter_caps(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("waiters");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 64usize;
    let payload = vec![0x55u8; 64];
    for i in 0..count {
        ctx.place(&format!("waiters/burst-{i}"), &format!("ITEM_WAITER_{i}"), &payload)?;
    }

    ctx.kill_daemon();
    let outcome = (|| -> Result<(), String> {
        let report = run_burst(ctx, &dir, count, 0x55)?;
        let parked = report.waits_ms.iter().filter(|ms| **ms > 5_000).count();
        let at_once = report.waits_ms.iter().filter(|ms| **ms <= 5_000).count();
        checks.note(
            ctx.fs,
            "waiter caps",
            &format!(
                "with the daemon gone and the root still registered, {count} opens: {parked} \
                 waited the full DAEMON_WAIT (they held a slot) and {at_once} were refused at \
                 once; {report}"
            ),
        );
        if report.answered != count {
            return Err(format!("{} opener(s) never returned", count - report.answered));
        }
        if report.ok != 0 || report.wrong != 0 {
            return Err(format!(
                "{} open(s) succeeded with no daemon to fill them",
                report.ok + report.wrong
            ));
        }
        if parked == 0 {
            return Err("no open waited for the daemon at all; the caps cannot be read off".into());
        }
        if parked >= count {
            return Err(format!(
                "all {count} opens parked: waiting is not bounded, and one user can consume the \
                 whole worker pool"
            ));
        }
        Ok(())
    })();

    if !ctx.daemon_connected() {
        ctx.connect_daemon()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Review item 8, first half: several thousand concurrent opens. The property
/// is not "everything succeeds" — the pool is bounded on purpose, and `EAGAIN`
/// is a real answer that means "try that again" — it is that **every** opener
/// is answered, that none of them reads zeros, and that a retry of the refused
/// ones then works.
pub(crate) fn burst(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("burst");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 3000usize;
    let payload = vec![0x33u8; 4096];
    for i in 0..count {
        ctx.place(&format!("burst/burst-{i}"), &format!("ITEM_BURST_{i}"), &payload)?;
    }

    // Each cause of a refusal, counted through the throttle. The
    // outbox has had two wordings: "is not draining them" while it could fill
    // with requests, "could not be queued" since its request capacity is the
    // credit; and "hydrations outstanding on its connection" is the refusal
    // for want of credit that removed. All are counted so that
    // this scenario says the same thing about a helper from either side of
    // those changes.
    let causes = |log: &Path| {
        (
            occurrences_in_log(log, "workers busy and"),
            occurrences_in_log(log, "is not draining them")
                + occurrences_in_log(log, "could not be queued"),
            occurrences_in_log(log, "hydrations outstanding on its connection"),
        )
    };
    let log = ctx.helper.lock().unwrap().log.clone();
    let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
    let (queue_full_before, outbox_full_before, saturated_before) = causes(&log);
    let gave_up_before = count_in_log(&log, "cannot write to a daemon");
    let silent_before = count_in_log(&log, "liveness window");
    let stranded_before = count_in_log(&log, "went away with");
    let fetches_before = ctx.fetches();
    let report = run_burst(ctx, &dir, count, 0x33)?;
    if !report.errors.is_empty() {
        // A throttled count is written when its interval is up, not when the
        // refusal happens; the last one of a burst arrives seconds later.
        std::thread::sleep(THROTTLE_SETTLE);
    }
    let (queue_full, outbox_full, saturated) = causes(&log);
    let queue_full = queue_full - queue_full_before;
    let outbox_full = outbox_full - outbox_full_before;
    let saturated = saturated - saturated_before;
    let gave_up = count_in_log(&log, "cannot write to a daemon") - gave_up_before;
    let silent = count_in_log(&log, "liveness window") - silent_before;
    let stranded = count_in_log(&log, "went away with") - stranded_before;
    let started_fills = ctx.fetches() - fetches_before;
    checks.note(ctx.fs, "burst", &format!("{count} concurrent opens: {report}"));
    checks.note(
        ctx.fs,
        "burst",
        &format!(
            "the daemon was asked to start {started_fills} fill(s) and completed {}",
            report.ok
        ),
    );
    // A burst is backpressure, not a wedged daemon, and must not
    // cost the connection. When it does, what the helper said about it is the
    // only account of *why* — its writer giving up for silence, its writer
    // finding the socket gone, or the daemon hanging up — so it is printed.
    let lost_connection = if gave_up > 0 || stranded > 0 {
        checks.note(
            ctx.fs,
            "burst",
            &format!(
                "the CONNECTION WAS LOST: the helper's writer gave up {gave_up} time(s) ({silent} \
                 of them for a whole liveness window of silence) and the helper stranded the \
                 connection's hydrations {stranded} time(s), denying every opener already \
                 enrolled on it EIO"
            ),
        );
        // Only the lines about the connection itself: once it is gone, every
        // open still queued in the pool is refused with a line of its own,
        // and thousands of those bury the one line that says why.
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        for line in text
            .lines()
            .skip(lines_before)
            .filter(|l| {
                ["went away with", "cannot write to a daemon", "connection ended", "liveness"]
                    .iter()
                    .any(|needle| l.contains(needle))
            })
            .take(20)
        {
            println!("  | {line}");
        }
        // The link this side holds is now a link to nothing; every later open
        // would wait the full `DAEMON_WAIT` and then be denied.
        ctx.kill_daemon();
        ctx.connect_daemon()?;
        true
    } else {
        false
    };
    checks.note(
        ctx.fs,
        "burst",
        &format!(
            "refusals by cause: the worker queue was full {queue_full} time(s) \
             (EVENT_WORKERS/EVENT_QUEUE_DEPTH), the connection already had \
             MAX_OUTSTANDING_HYDRATIONS = {} in flight {saturated} time(s), the daemon's outbox \
             refused {outbox_full} time(s)",
            konedrive_proto::MAX_OUTSTANDING_HYDRATIONS,
        ),
    );
    if report.answered != count {
        return Err(format!("{} of {count} openers never returned at all", count - report.answered));
    }
    if report.wrong != 0 {
        return Err(format!(
            "{} opener(s) were allowed onto a file that had not been filled",
            report.wrong
        ));
    }
    if !ctx.helper_alive() {
        return Err("the helper did not survive the burst".into());
    }
    if lost_connection {
        return Err(format!(
            "the burst cost the daemon its connection, and {} enrolled opener(s) were denied EIO \
             (Ruling H119: a burst is backpressure, not a wedged daemon)",
            report.errors.get(&libc::EIO).copied().unwrap_or(0)
        ));
    }
    // Beyond the credit an opener waits for the daemon; it is not
    // refused. The only refusal a burst may still meet is a bound that is
    // genuinely exhausted — the worker pool's queue — so every EAGAIN must be
    // one of those, and nothing else may be refused at all.
    if saturated > 0 {
        return Err(format!(
            "{saturated} opener(s) were refused EAGAIN because their connection already had \
             MAX_OUTSTANDING_HYDRATIONS in flight: beyond the credit an opener must wait, not \
             fail (Ruling H124); {report}"
        ));
    }
    let refused = report.errors.get(&libc::EAGAIN).copied().unwrap_or(0);
    let others: BTreeMap<i32, usize> =
        report.errors.iter().filter(|(e, _)| **e != libc::EAGAIN).map(|(e, n)| (*e, *n)).collect();
    if !others.is_empty() {
        return Err(format!("openers were denied {others:?} (errno -> openers); {report}"));
    }
    if outbox_full > 0 {
        return Err(format!(
            "the daemon's outbox refused {outbox_full} request(s); its request capacity is the \
             credit, so under the credit it cannot fill"
        ));
    }
    if refused > queue_full {
        return Err(format!(
            "{refused} opener(s) were refused EAGAIN, but the worker queue — the one bound that \
             may still refuse a burst — was full only {queue_full} time(s)"
        ));
    }

    // Everything that was refused must succeed when it is tried again: that
    // is what makes `EAGAIN` an answer rather than a failure.
    // Twenty, not two hundred: a retry that is itself refused takes
    // `DAEMON_WAIT` to answer, and a loop of those is how this scenario turned
    // a ten-second measurement into an hour-long hang.
    let mut still_failing = Vec::new();
    for i in report.failed_indices.iter().take(20) {
        let path = dir.join(format!("burst-{i}"));
        match ctx.read(&path) {
            Ok(content) if content == payload => {}
            Ok(_) | Err(_) => {
                still_failing.push(*i);
                break;
            }
        }
    }
    if !still_failing.is_empty() {
        return Err(format!(
            "a refused opener (index {}) still could not be served on a retry",
            still_failing[0]
        ));
    }
    Ok(())
}

/// Review item 8, second half, and the worst outcome this component has.
/// `fanotify(7)`: *"Upon close(2), outstanding permission events will be set
/// to allowed"* — quoted in the design and never measured. So it is measured
/// here: the helper is killed with openers suspended, and what those openers
/// then read is recorded.
pub(crate) fn helper_death_allows(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("death");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 200usize;
    let payload = vec![0x44u8; 4096];
    for i in 0..count {
        ctx.place(&format!("death/burst-{i}"), &format!("ITEM_DEATH_{i}"), &payload)?;
    }
    // Long enough that every opener is still suspended when the helper dies.
    ctx.set_source_delay(Duration::from_secs(30));

    let killer = {
        let pid = ctx.helper_pid();
        let source = Arc::clone(&ctx.source);
        let before = ctx.fetches();
        std::thread::spawn(move || {
            // Killed once openers are genuinely suspended, not after a fixed
            // sleep: what is being measured is what the kernel does to
            // *outstanding* permission events.
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && source.fetches() <= before {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(500));
            // SAFETY: a plain kill on a pid this process owns.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        })
    };
    let report = run_burst(ctx, &dir, count, 0x44)?;
    let _ = killer.join();

    checks.note(
        ctx.fs,
        "helper death",
        &format!(
            "{count} concurrent opens, of which those still suspended when the helper was \
             killed are the \"read-wrong\" ones ({} — every opener stays suspended now, at \
             most MAX_OUTSTANDING_HYDRATIONS of them sent to the daemon and the rest waiting in \
             the helper for credit): {report} — \
             every \"read-wrong\" open is an application that was handed an unfilled \
             placeholder, which is what fanotify(7) means by \"outstanding permission events \
             will be set to allowed\"",
            report.wrong,
        ),
    );
    if report.answered != count {
        return Err(format!(
            "{} opener(s) were left suspended even after the group fd closed",
            count - report.answered
        ));
    }

    ctx.source.reset();
    // `kill_daemon` first: the link is already dead, and the tasks holding it
    // have to go before a new one is made.
    ctx.kill_daemon();
    {
        let mut helper = ctx.helper.lock().unwrap();
        let _ = helper.child.wait();
        let _ = std::fs::remove_file(SOCKET_PATH);
        helper.child = spawn_helper(&helper.binary.clone(), &helper.log.clone(), None, None)?;
        helper.await_socket()?;
    }
    ctx.connect_daemon()?;
    let after = ctx.place("after-death.bin", "ITEM_AFTERDEATH", b"BACK")?;
    if ctx.read(&after)? != b"BACK" {
        return Err("interception did not come back after the helper was restarted".into());
    }
    Ok(())
}
