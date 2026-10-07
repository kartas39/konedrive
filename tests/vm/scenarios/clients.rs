use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use konedrive_fs::placeholder::State;
use konedrive_proto::{ToDaemon, ToHelper, PROTOCOL_VERSION};

use crate::child::raw_connect;
use crate::harness::{Checks, Ctx, Reader, Responder, dir_mark_present};
use crate::HOSTILE_UID;

/// `FAN_DENY | (errno << 24)` is accepted by the kernel for
/// eight values and refused for every other, and a refused response leaves the
/// opener suspended **forever** (`docs/kernel-behavior-7.2/interception.md` §5). The daemon reports errnos the helper does
/// not choose, so every value it could ever produce is swept here against a
/// live suspended opener, and the property asserted is the one that matters:
/// nobody is left unanswered.
pub(crate) fn errno_sweep(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("sweep.bin", "ITEM_SWEEP", b"SWEPT")?;
    let mut accepted: Vec<i32> = Vec::new();
    let mut delivered: BTreeMap<i32, i32> = BTreeMap::new();
    let mut unanswered: Vec<i32> = Vec::new();

    let mut candidates: Vec<i32> = (0..=133).collect();
    // Values a daemon has no business sending, and which must not corrupt the
    // response word either.
    candidates.extend([256, 512, 4095, -1, i32::MAX, i32::MIN]);

    let responder = Responder::start().map_err(|e| format!("cannot start the responder: {e}"))?;
    for errno in candidates {
        responder.answer_with(errno);
        // A fresh placeholder every time: a file the helper has already
        // allowed carries an ignore mark and raises no event at all.
        let name = format!("sweep-{}.bin", errno as i64 & 0xffff_ffff);
        let path = ctx
            .place(&name, "ITEM_SWEEP", b"SWEPT")
            .map_err(|e| format!("cannot place a sweep placeholder: {e}"))?;
        let reader = Reader::start(&ctx.exe, &path)?;
        match reader.get(Duration::from_secs(15)) {
            Ok(Ok(_)) => {
                accepted.push(errno);
            }
            Ok(Err(got)) => {
                delivered.insert(errno, got);
            }
            Err(_) => {
                unanswered.push(errno);
                // A suspended opener that is never answered holds an event fd
                // in the helper for the rest of the run; killing it does not
                // release the kernel's side, so the rest of the sweep is not
                // worth continuing.
                reader.kill();
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    drop(responder);
    let _ = path;
    // Hydrations go back to the daemon underneath, which the
    // rest of the suite needs.
    let back = ctx.place("sweep-after.bin", "ITEM_SWEEP_AFTER", b"THE DAEMON AGAIN")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Reader::start(&ctx.exe, &back)?.get(Duration::from_secs(60))? {
            Ok(content) if content == b"THE DAEMON AGAIN" => break,
            other if Instant::now() > deadline => {
                return Err(format!("after the sweep, the daemon did not get hydrations back: {other:?}"))
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    checks.note(
        ctx.fs,
        "errno sweep",
        &format!(
            "{} value(s) let the open through, {} were delivered as an errno, {} left the opener \
             hanging",
            accepted.len(),
            delivered.len(),
            unanswered.len()
        ),
    );
    // Which errnos arrive unchanged is the interesting part: everything else
    // is clamped to EIO by `clamp_deny_errno`, or refused by the kernel and
    // rescued with a plain FAN_DENY (EPERM).
    let verbatim: Vec<i32> =
        delivered.iter().filter(|(asked, got)| *asked == *got).map(|(asked, _)| *asked).collect();
    checks.note(ctx.fs, "errno sweep", &format!("delivered verbatim: {verbatim:?}"));
    let as_eperm: Vec<i32> =
        delivered.iter().filter(|(_, got)| **got == libc::EPERM).map(|(a, _)| *a).collect();
    if !as_eperm.is_empty() {
        checks.note(
            ctx.fs,
            "errno sweep",
            &format!("arrived as EPERM (a plain FAN_DENY rescue): {as_eperm:?}"),
        );
    }

    if !unanswered.is_empty() {
        return Err(format!(
            "these errnos left a suspended opener with no answer at all: {unanswered:?}"
        ));
    }
    Ok(())
}

/// The socket is 0666 by design, so everything that protects
/// one user's hydrations from another has to be an authorisation check inside
/// the helper. Three of them, driven from a process running as another uid.
pub(crate) fn hostile_uid(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("victim.bin", "ITEM_VICTIM", b"VICTIM")?;
    ctx.set_source_delay(Duration::from_secs(3));
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, &path)?;
    ctx.wait_for_fetch(before, Duration::from_secs(20))?;

    // The suite's own binary lives under the developer's home directory,
    // which another uid cannot even traverse; a copy on the guest's tmpfs is
    // what a stranger on this machine would actually have.
    let hostile_exe = Path::new("/run/konedrive-hostile-client");
    std::fs::copy(&ctx.exe, hostile_exe)
        .map_err(|e| format!("cannot stage the hostile client: {e}"))?;
    std::fs::set_permissions(
        hostile_exe,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .map_err(|e| e.to_string())?;
    let output = Command::new(hostile_exe)
        .arg("--hostile")
        .arg(&ctx.sync_root().root_id)
        .env("KONEDRIVE_VICTIM_ROOT", &ctx.root)
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the hostile client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    for line in said.lines() {
        checks.note(ctx.fs, "hostile client", line);
    }

    // The victim's hydration must complete normally: the guessed
    // `HydrateDone`s must not have force-allowed it onto an unfilled file.
    let content = reader.content(Duration::from_secs(30))?;
    if content != b"VICTIM" {
        return Err(format!(
            "another uid changed the outcome of a hydration: the reader got {content:?}"
        ));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }

    if !said.contains("UNREGISTER errno=1") {
        return Err(format!("UnregisterRoot from another uid was not refused EPERM: {said:?}"));
    }
    if !said.contains("UNMARKDIR errno=1") {
        return Err(format!("UnmarkDir from another uid was not refused EPERM: {said:?}"));
    }

    // And the root is still registered and still marked.
    let root_ino = ctx.ino_of(&ctx.root)?;
    if !dir_mark_present(ctx.helper_pid(), root_ino) {
        return Err("another uid managed to take the mark off the root directory".into());
    }
    ctx.source.reset();
    let after = ctx.place("after-hostile.bin", "ITEM_AFTERHOSTILE", b"STILL WORKS")?;
    if ctx.read(&after)? != b"STILL WORKS" {
        return Err("interception stopped working after the hostile client ran".into());
    }
    Ok(())
}

/// `SO_PEERCRED` on a `SOCK_SEQPACKET` socket must
/// report the pid the fanotify event reports, or the daemon's narrow exemption
/// either does not fire — deadlocking startup recovery — or fires
/// for the wrong process. It is observable from here precisely because the
/// exemption is: this process holds the connection, so its own open of an
/// `online-only` placeholder must go straight through with no fetch, while the
/// same open from any other process must be intercepted and filled.
/// Every connection costs the helper
/// two threads and about three descriptors, and the socket is 0666: any
/// local user could open connections until the helper ran out of
/// descriptors, and from then on every intercepted open on the machine was
/// denied. A uid gets a bounded number of live connections; beyond it a
/// connection is closed at once, and nobody else's is touched.
pub(crate) fn connections_per_uid_are_capped(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let hostile_exe = Path::new("/run/konedrive-hostile-client");
    std::fs::copy(&ctx.exe, hostile_exe)
        .map_err(|e| format!("cannot stage the hostile client: {e}"))?;
    std::fs::set_permissions(hostile_exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let output = Command::new(hostile_exe)
        .arg("--connections")
        .arg("40")
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the hostile client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    checks.note(ctx.fs, "connections per uid", &said);
    let greeted: usize = said
        .strip_prefix("GREETED ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("the client said {said:?}"))?;
    let after = ctx.place("after-connections.bin", "ITEM_AFTER_CONNECTIONS", b"STILL SERVED")?;
    if ctx.read(&after)? != b"STILL SERVED" {
        return Err("interception stopped working after one uid's connections".into());
    }
    if greeted >= 40 {
        return Err(format!(
            "{said}: one uid holds every connection it asks for, so any local user can run the \
             helper out of descriptors"
        ));
    }
    Ok(())
}

pub(crate) fn peercred_pid_matches_event_pid(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("exempt.bin", "ITEM_EXEMPT", b"EXEMPT")?;
    let before = ctx.fetches();
    let mut content = Vec::new();
    File::open(&path)
        .map_err(|e| format!("the daemon's own open was not exempt: {e}"))?
        .read_to_end(&mut content)
        .map_err(|e| e.to_string())?;
    if ctx.fetches() != before {
        return Err(format!(
            "the daemon's own open was hydrated ({} fetch(es)), so it was not exempt",
            ctx.fetches() - before
        ));
    }
    if !content.iter().all(|b| *b == 0) {
        return Err("an exempt open of an online-only placeholder returned content".into());
    }
    match ctx.state_of(&path)? {
        Some(State::OnlineOnly) => {}
        other => return Err(format!("the exempt open changed the state to {other:?}")),
    }

    // The converse: another process is not exempt.
    let filled = ctx.read(&path)?;
    if filled != b"EXEMPT" {
        return Err(format!("a non-daemon open got {filled:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!(
            "a non-daemon open caused {} fetch(es)",
            ctx.fetches() - before
        ));
    }
    Ok(())
}

/// `PR1`. A peer whose `Hello` names another protocol version is not served:
/// the helper closes the connection, with no `Ack`. It used to answer
/// `EPROTO` and go on taking that peer's requests. The daemon underneath is
/// untouched.
pub(crate) fn another_version_is_closed(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let mut channel = raw_connect().map_err(|e| format!("cannot connect: {e}"))?;
    channel.get_ref().set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    match channel.recv::<ToDaemon>() {
        Ok((ToDaemon::Welcome { version: PROTOCOL_VERSION }, _)) => {}
        other => return Err(format!("not greeted with the helper's version: {other:?}")),
    }
    channel
        .send(&ToHelper::Hello { version: PROTOCOL_VERSION + 1 }, None)
        .map_err(|e| e.to_string())?;
    match channel.recv::<ToDaemon>() {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => {
            return Err(format!(
                "a Hello with another version was answered, or the connection kept: {other:?}"
            ))
        }
    }
    drop(channel);
    if !ctx.helper_alive() {
        return Err("the helper exited over a Hello with another version".into());
    }
    // The uid's daemon, underneath that connection while it lived, has the
    // hydrations again.
    let path = ctx.place("after-version.bin", "ITEM_AFTERVERSION", b"SAME VERSION")?;
    if ctx.read(&path)? != b"SAME VERSION" {
        return Err("the daemon's connection did not survive a stranger's Hello".into());
    }
    Ok(())
}
