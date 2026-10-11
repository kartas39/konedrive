//! The daemon's own service for one account, with a folder served through FUSE, over the fake
//! OneDrive: `fuse-service <state directory> [--onedrive <address>] [--read-write]`. It prints
//! `ready` and then takes one command a line on its input, answering each with
//! `done <line>: <what came of it>`.
//! SIGTERM stops it as it stops the daemon: it prints `stopped: <how the stop ended>` and exits.
//!
//! The fake OneDrive is the service's own, and takes its commands here
//! (`fuse_probe::fake_onedrive_commands`), unless `--onedrive` names one that runs as a process
//! of its own (`fake-onedrive`), which outlives a service that is killed.
//!
//! The account's mode is the one `config.toml` in the state directory records, read-only at
//! first; `--read-write` records read-write there before the folder comes up. `RUST_LOG` set,
//! the daemon's log goes to the standard error.
//!
//! The folder's: `register <path>`, `forget`, `refresh`, `hydrate <path>`, `dehydrate <path>`,
//! `free-up <path>`, `free-up-space`, `pin <path>`, `unpin <path>`, `state <path>`,
//! `menu <path>`, `status`, `activity`, `staged` (the downloads that never ended).
//!
//! The account's mode: `mode read-write`; `mode read-only`, the switch as the account makes it at
//! once (with nothing unsent, or forced): what is unsent stays in the folder, held, and is sent
//! when the account is read-write again; `mode read-only keep`, the mode the account runs in
//! turned alone, as a sign-out or a narrower grant turns it. The switch that waits for what is
//! unsent is the account's own (`AccountService`), which this program has none of.
//!
//! What is unsent: `outbox` (the upload queue's entries), `outbox-status` (its counts), `not-uploaded`,
//! `conflicts`, `pause`, `resume`.

use std::path::{Path, PathBuf};

use std::sync::Arc;
use std::time::Duration;

use fuse_probe::fake_onedrive_commands::FakeOneDriveCommands;
use konedrive_graph::drive::{DriveClient, RetryPolicy};
use konedrive_graph::token::StaticToken;
use konedrived::account::state::{AccountSnapshot, SignInState, StateHandle};
use konedrived::account::PendingUploads;
use futures_util::FutureExt;
use konedrived::config::{ConfigError, DriveId, Mode};
use konedrived::daemon::stop;
use konedrived::account_folder::SyncService;
use konedrived::account_folder::testing;
use konedrived::account_folder::wiring::SyncPaths;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    // `fuse-service --tidy <folder>`: what the unit runs after the daemon's end.
    if std::env::args().nth(1).as_deref() == Some("--tidy") {
        konedrived::mount::tidy_folder(Path::new(&std::env::args().nth(2).expect("a folder")));
        return;
    }
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let state = PathBuf::from(arguments.first().expect("a state directory"));
    let onedrive_address = arguments.iter().position(|a| a == "--onedrive").map(|at| arguments[at + 1].clone());
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).with_ansi(false).init();
    }
    let mut own_onedrive = match onedrive_address {
        Some(_) => None,
        None => Some(FakeOneDriveCommands::start().await),
    };
    let drive = match (&onedrive_address, &own_onedrive) {
        (Some(address), _) => to_drive_client(address),
        (None, Some(onedrive)) => onedrive.graph().client(),
        (None, None) => unreachable!(),
    };
    let paths = SyncPaths { tree_db: state.join("fuse.sqlite"), thumbnails: None };
    // The folder is recorded in a `config.toml` that stays: a second run finds it there, as a
    // daemon started again does, and brings it up, in the mode recorded with it.
    let persist = testing::persist(&state.join("config.toml"));
    let record_mode = |mode: Mode| {
        let recorded = persist.store.update_account(&persist.account, |account| {
            account.mode = mode;
            account.drive_id = DriveId::new(FAKE_DRIVE);
            Ok::<_, ConfigError>(())
        });
        recorded.expect("config.toml written");
    };
    if arguments.iter().any(|a| a == "--read-write") {
        record_mode(Mode::ReadWrite);
    }
    let mode = persist.store.account(&persist.account).map_or(Mode::ReadOnly, |account| account.mode);
    // What the write gate asks of a read-write account, besides `config.toml`: a sign-in that
    // allows changes, last seen to reach the drive recorded.
    let account = StateHandle::new(AccountSnapshot {
        state: SignInState::SignedIn,
        mode,
        granted_scopes: "Files.ReadWrite offline_access".into(),
        live_drive: FAKE_DRIVE.into(),
        ..AccountSnapshot::default()
    });
    let service = testing::wiring().account(account.clone()).persist(persist.clone()).onedrive(drive, paths).build();
    // The folder takes the account's mode before it is brought up, as the accounts manager
    // has it.
    service.follow_mode(mode).await;
    service.resume().await;
    // From now on SIGTERM stops the service as it stops the daemon (`konedrived`'s `main`): the one call of the
    // daemon's end for the account's folder, within the stop's bound.
    let mut signals = stop::Signals::install().expect("the handlers of the signals");
    println!("ready");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = tokio::select! {
            _ = signals.next() => {
                let ended = stop::wind_down(service.end_at_daemon_stop().map(drop), stop::STOP_BOUND, std::future::pending()).await;
                println!("stopped: {ended:?}");
                std::process::exit(0);
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                _ => break,
            },
        };
        let words: Vec<&str> = line.split_whitespace().collect();
        let said = |r: Result<String, String>| r.unwrap_or_else(|e| format!("refused: {e}"));
        let path = |p: &&str| Path::new(*p).to_owned();
        if let Some(onedrive) = &mut own_onedrive {
            if let Some(answer) = onedrive.answer(&words).await {
                println!("done {line}: {answer}");
                continue;
            }
        }
        let answer = match words.as_slice() {
            ["staged"] => format!("{:?}", testing::staged(&service).await),
            ["register", p] => said(service.register_root(&path(p)).await.map(|()| "registered".into()).map_err(|e| e.to_string())),
            ["forget"] => said(service.unregister_root().await.map(|()| "forgotten".into()).map_err(|e| e.to_string())),
            ["refresh"] => said(service.refresh().await.map(|()| "refreshed".into()).map_err(|e| e.to_string())),
            ["hydrate", p] => said(service.hydrate_now(&path(p)).await.map(|()| "downloaded".into()).map_err(|e| e.to_string())),
            ["dehydrate", p] => said(service.dehydrate(&path(p)).await.map(|()| "freed".into()).map_err(|e| e.to_string())),
            ["free-up", p] => said(service.free_up(&[path(p)]).await.map(|f| format!("{f:?}")).map_err(|e| e.to_string())),
            ["free-up-space"] => said(service.free_up_space().await.map(|f| format!("{f:?}")).map_err(|e| e.to_string())),
            ["pin", p] => said(service.pin(&[path(p)]).await.map(|n| format!("{n} pinned")).map_err(|e| e.to_string())),
            ["unpin", p] => said(service.unpin(&[path(p)]).await.map(|n| format!("{n} unpinned")).map_err(|e| e.to_string())),
            ["state", p] => service.item_state(&path(p)).await,
            ["menu", p] => {
                let part = service.menu_part(&[path(p)]).await;
                format!("taken {:?}, kept by {:?}, free-up refused {:?}", part.taken, part.kept_by, part.free_up_refused)
            }
            ["status"] => {
                let s = service.state().get();
                format!(
                    "state {:?}, listed {}, placed {}, pinned {}, local bytes {}, last checked {}, last error {:?}",
                    s.folder.root_state, s.cycle.items_listed, s.cycle.items_placed, s.local.pinned_count, s.local.local_bytes, s.cycle.last_checked > 0, s.folder.last_error
                )
            }
            ["activity"] => said(service.recent_activity(20).await.map(|recent| recent.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(" | ")).map_err(|e| e.to_string())),
            ["mode", "read-write"] => {
                record_mode(Mode::ReadWrite);
                follow(&service, &account, Mode::ReadWrite).await
            }
            // As the account makes its switch, at once (nothing unsent, or forced): `config.toml`, its state, and
            // then the folder told to hold what is unsent.
            ["mode", "read-only"] => {
                record_mode(Mode::ReadOnly);
                account.update(|snapshot| snapshot.mode = Mode::ReadOnly);
                service.hold_unsent_changes().await;
                service.mode().as_str().to_owned()
            }
            ["mode", "read-only", "keep"] => follow(&service, &account, Mode::ReadOnly).await,
            ["outbox"] => said(
                service
                    .outbox(0)
                    .await
                    .map(|rows| rows.iter().map(|row| format!("{} {} {} {}", row.kind, row.path, row.state, row.reason).trim_end().to_owned()).collect::<Vec<_>>().join(" | "))
                    .map_err(|e| e.to_string()),
            ),
            ["outbox-status"] => {
                let outbox = service.state().get().outbox;
                format!("pending {}, blocked {}, note {:?}", outbox.pending_count, outbox.blocked_count, outbox.note)
            }
            ["not-uploaded"] => said(
                service.not_uploaded().await.map(|kept| kept.iter().map(|k| format!("{} {}", k.path, k.reason)).collect::<Vec<_>>().join(" | ")).map_err(|e| e.to_string()),
            ),
            ["conflicts"] => said(
                service
                    .conflicts()
                    .await
                    .map(|rows| rows.iter().map(|row| format!("{:?} {} {}", row.kind, row.original, row.rescued)).collect::<Vec<_>>().join(" | "))
                    .map_err(|e| e.to_string()),
            ),
            ["pause"] => said(service.pause_syncing(0).await.map(|()| "paused".into()).map_err(|e| e.to_string())),
            ["resume"] => said(service.resume_syncing().await.map(|()| "resumed".into()).map_err(|e| e.to_string())),
            _ => "not a command".into(),
        };
        println!("done {line}: {answer}");
    }
}

/// The drive the fake OneDrive answers `GET /me/drive` with.
const FAKE_DRIVE: &str = "D";

/// The client of the fake OneDrive that listens at `address`, as the fake makes its own.
fn to_drive_client(address: &str) -> DriveClient {
    let base = url::Url::parse(&format!("{address}/")).expect("the fake OneDrive's address");
    DriveClient::new(base, Arc::new(StaticToken::new("T")))
        .expect("a client of the fake OneDrive")
        .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(20) })
}

/// The account runs in `mode` from now on, and its folder follows it; the mode the folder is
/// in afterwards.
async fn follow(service: &SyncService, account: &StateHandle, mode: Mode) -> String {
    account.update(|snapshot| snapshot.mode = mode);
    service.follow_mode(mode).await;
    service.mode().as_str().to_owned()
}
