use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use konedrive_proto::ToDaemon;

use crate::burst::run_burst;
use crate::child::raw_connect;
use crate::harness::{Checks, Ctx, Reader, count_in_log, dir_mark_present};
use crate::HOSTILE_UID;

pub(crate) fn daemon_death_denies(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("orphan.bin", "ITEM_ORPHAN", b"NEVER ARRIVES")?;
    ctx.set_source_delay(Duration::from_secs(20));
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, &path)?;
    // The reader has to be a *stranded waiter* before the daemon dies, not an
    // open that merely has not happened yet: those two are indistinguishable
    // from the outside and the second one legitimately takes `DAEMON_WAIT`.
    let outcome = (|| -> Result<(), String> {
        ctx.wait_for_fetch(before, Duration::from_secs(20))?;
        // `kill_daemon` drops every `HelperLink` handle, which is what a
        // daemon process dying does to its socket. Whether the
        // helper *notices* is the thing under test, and it has no interface
        // but its journal.
        let log = ctx.helper.lock().unwrap().log.clone();
        let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
        // Counted here, before the kill, not inside the watcher: the watcher
        // thread can start after the helper has already noticed — once the
        // disconnect really happens, it takes about a millisecond — and a
        // baseline taken then already contains the line it is waiting for.
        let before = count_in_log(&log, "went away with");
        let watching = std::thread::spawn({
            let log = log.clone();
            move || {
                let started = Instant::now();
                while started.elapsed() < Duration::from_secs(20) {
                    if count_in_log(&log, "went away with") > before {
                        return Some(started.elapsed());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                None
            }
        });
        ctx.kill_daemon();
        let noticed = watching.join().unwrap_or(None);
        let Some(noticed) = noticed else {
            // What the helper said about its connections meanwhile is the
            // only way to tell "the socket never closed" from "it closed,
            // but the opener's job was not on it".
            let said: Vec<String> = std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .skip(lines_before)
                .filter(|l| l.contains("connection") || l.contains("daemon"))
                .map(str::to_owned)
                .collect();
            return Err(format!(
                "the helper never noticed the daemon was gone: dropping every `HelperLink` \
                 handle did not tear the connection down, so the suspended opener was never \
                 denied; the helper said: {said:?}"
            ));
        };
        let errno = reader.errno(Duration::from_secs(30)).map_err(|e| {
            format!("{e} (the helper noticed the disconnect after {noticed:?})")
        })?;
        if errno != libc::EIO {
            return Err(format!("the reader got errno {errno}, not EIO"));
        }
        if !ctx.helper_alive() {
            return Err("the helper exited when the daemon did".into());
        }
        Ok(())
    })();
    reader.kill();
    ctx.source.reset();
    // Whatever happened, a suite without a daemon is a suite in which the
    // harness's own opens are no longer exempt and block for 30 s each.
    if !ctx.daemon_connected() {
        ctx.connect_daemon()?;
    }
    outcome
}

/// The disconnect half. A hydration waiting for credit belongs to
/// its connection as much as one already sent, and goes with it. With a slow
/// source, twice the credit's worth of openers are suspended — at most
/// `MAX_OUTSTANDING_HYDRATIONS` of them sent to the daemon, the rest enrolled
/// in the helper and not yet sent — and then the daemon dies. Every one must
/// be denied `EIO`: none left suspended, none let through onto an unfilled
/// file, and none refused `EAGAIN` up front, which is what the 65th opener got
/// before the queue existed.
pub(crate) fn daemon_death_denies_queued(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("queued");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 2 * konedrive_proto::MAX_OUTSTANDING_HYDRATIONS + 8;
    let payload = vec![0x66u8; 4096];
    for i in 0..count {
        ctx.place(&format!("queued/burst-{i}"), &format!("ITEM_QUEUED_{i}"), &payload)?;
    }
    ctx.set_source_delay(Duration::from_secs(20));
    let log = ctx.helper.lock().unwrap().log.clone();
    let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
    let before = ctx.fetches();

    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            // Once the first fill has begun the burst's openers are all
            // arriving; a second and a half more is time enough for every one
            // of them to be enrolled, and nowhere near the twenty seconds the
            // first fill takes.
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && ctx.fetches() <= before {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(1500));
            ctx.kill_daemon();
        });
        run_burst(ctx, &dir, count, 0x66)
    });
    ctx.source.reset();
    let outcome = (|| -> Result<(), String> {
        let report = report?;
        // What the helper held for the connection when it went: the jobs its
        // disconnect guard denied. Every opener should have been one of them.
        let stranded: Vec<String> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .skip(lines_before)
            .filter(|l| l.contains("went away with"))
            .map(str::to_owned)
            .collect();
        checks.note(
            ctx.fs,
            "daemon death, queued",
            &format!("{count} openers, daemon killed: {report}; the helper said: {stranded:?}"),
        );
        if report.answered != count {
            return Err(format!("{} opener(s) were never answered", count - report.answered));
        }
        if report.ok != 0 || report.wrong != 0 {
            return Err(format!(
                "{} opener(s) got through with no daemon left to fill them ({} of them onto an \
                 unfilled file)",
                report.ok + report.wrong,
                report.wrong
            ));
        }
        let eio = report.errors.get(&libc::EIO).copied().unwrap_or(0);
        if eio != count {
            return Err(format!(
                "every opener should have been denied EIO with its connection, but they got \
                 {:?} (errno -> openers)",
                report.errors
            ));
        }
        let held = stranded.iter().find_map(|line| {
            let after = line.split("went away with ").nth(1)?;
            after.split_whitespace().next()?.parse::<usize>().ok()
        });
        if held != Some(count) {
            return Err(format!(
                "the connection held {held:?} hydrations when it went, not all {count}: the \
                 openers were not all enrolled before the daemon died, so this run did not \
                 measure the queued ones"
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

/// A process of the daemon's own uid — a CLI, a second instance, a
/// probe — connects after it and then goes away. The helper routes a uid's
/// hydrations to its newest connection, so while the newer one is there it is
/// where they go; when it leaves, the live daemon underneath must get them
/// back.
///
/// Before the per-uid stack it did not: the newer connection replaced the
/// uid's one registration, its own cleanup then removed it, and the live
/// daemon — whose socket was still open, so it had no reason to reconnect —
/// was left unregistered. Every later open waited `DAEMON_WAIT` and was denied
/// `EIO`, until the daemon restarted. No scheduling luck was needed.
pub(crate) fn transient_connection_hands_back(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let routed = ctx.place("transient/routed.bin", "ITEM_TRANSIENT_ROUTED", b"ROUTED")?;
    let after =
        ctx.place("transient/after.bin", "ITEM_TRANSIENT_AFTER", b"THE LIVE DAEMON AGAIN")?;
    let outcome = (|| -> Result<(), String> {
        let mut transient =
            raw_connect().map_err(|e| format!("cannot open a second connection: {e}"))?;
        transient
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        match transient.recv::<ToDaemon>() {
            Ok((ToDaemon::Welcome { .. }, _)) => {}
            other => return Err(format!("the second connection was not greeted: {other:?}")),
        }
        // The newer connection must really have become the one hydrations go
        // to, or its leaving would hand nothing back and this would pass for
        // the wrong reason.
        let before = ctx.fetches();
        let stranded = Reader::start(&ctx.exe, &routed)?;
        match transient.recv::<ToDaemon>() {
            Ok((ToDaemon::HydrateRequest { .. }, Some(_))) => {}
            other => {
                return Err(format!(
                    "the newer connection was never asked to hydrate (it got {other:?}, and the \
                     live daemon fetched {} time(s)): it never became the one hydrations go to",
                    ctx.fetches() - before
                ))
            }
        }
        drop(transient);
        // Its own opener goes with it: a connection's hydrations are denied
        // when it disconnects, whoever sits underneath.
        let errno = stranded.errno(Duration::from_secs(10))?;
        if errno != libc::EIO {
            return Err(format!("the newer connection's own opener got errno {errno}, not EIO"));
        }

        let started = Instant::now();
        let got = Reader::start(&ctx.exe, &after)?.get(Duration::from_secs(60))?;
        let took = started.elapsed();
        checks.note(
            ctx.fs,
            "transient connection",
            &format!(
                "after the newer same-uid connection left, the next open took {took:?} and {}",
                match &got {
                    Ok(content) => format!("read {} bytes", content.len()),
                    Err(errno) => format!("was denied errno {errno}"),
                }
            ),
        );
        match got {
            Ok(content) if content == b"THE LIVE DAEMON AGAIN" => {}
            Ok(content) => return Err(format!("the open read {content:?}")),
            Err(errno) => {
                return Err(format!(
                    "after a newer same-uid connection came and went, an open was denied errno \
                     {errno} after {took:?}: the live daemon underneath no longer receives \
                     hydrations, and nothing will make it reconnect"
                ))
            }
        }
        if ctx.fetches() != before + 1 {
            return Err(format!(
                "the live daemon fetched {} time(s), not once",
                ctx.fetches() - before
            ));
        }
        Ok(())
    })();
    if outcome.is_err() {
        // Whatever the helper now believes about this uid, the rest of the
        // suite needs a daemon that hydrations actually reach.
        ctx.kill_daemon();
        ctx.connect_daemon()?;
    }
    let _ = std::fs::remove_dir_all(ctx.root.join("transient"));
    outcome
}

/// A peer that sends faster than it reads its replies — two
/// thousand requests before it reads a single `Ack` — is backpressure, not a
/// dead peer, and keeps its connection. It used to lose it: `serve_one` ended
/// any connection whose `Ack` did not fit in the outbox, which a peer reaches
/// by being slow to read for a few hundred requests, whatever it is.
///
/// The peer is a uid with no root, so that nothing it does touches the routing
/// of the suite's own daemon; `serve_one` is the same code for every
/// connection.
pub(crate) fn pipelined_acks(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    const COUNT: usize = 2000;
    // The suite's binary lives where another uid cannot traverse; the same
    // copy the hostile client uses.
    let client = Path::new("/run/konedrive-pipeline-client");
    std::fs::copy(&ctx.exe, client).map_err(|e| format!("cannot stage the client: {e}"))?;
    std::fs::set_permissions(client, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let output = Command::new(client)
        .arg("--pipeline")
        .arg(COUNT.to_string())
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    checks.note(ctx.fs, "pipelined Acks", &said.lines().collect::<Vec<_>>().join("; "));
    let acks = said
        .lines()
        .find_map(|l| l.strip_prefix("ACKS "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(|| format!("the client reported no count: {said:?}"))?;
    if acks != COUNT {
        return Err(format!(
            "a peer that was slow to read got {acks} of its {COUNT} Acks and then lost its \
             connection: the helper ended it for backpressure ({said:?})"
        ));
    }
    if !said.lines().any(|l| l == "ALIVE") {
        return Err(format!("the connection did not survive the pipelining: {said:?}"));
    }
    if !ctx.helper_alive() {
        return Err("the helper did not survive a pipelining peer".into());
    }
    Ok(())
}

pub(crate) fn helper_restart_remarks(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("nested/deep/after-restart.bin", "ITEM_RESTART", b"RESTARTED")?;
    let dir_ino = ctx.ino_of(&ctx.root.join("nested/deep"))?;
    ctx.restart_helper()?;
    if !dir_mark_present(ctx.helper_pid(), dir_ino) {
        return Err(format!(
            "the nested directory (ino {dir_ino}) carries no mark after the startup walk"
        ));
    }
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    if content != b"RESTARTED" {
        return Err(format!("the reader got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}
