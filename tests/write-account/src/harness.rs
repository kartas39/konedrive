//! One run: the guards that must hold before anything is written (`docs/design/writes.md` §12, guards 1,
//! 2 and 5), the checks, and the cleanup.

use std::sync::Arc;
use std::time::Duration;

use konedrived::config::{Config, DriveId};
use konedrive_graph::drive::{DeltaFrom, DeltaNext, DriveClient};
use konedrive_graph::token::StaticToken;
use serde_json::Value;
use url::Url;

use crate::args::Only;
use crate::checks::{self, Outcome};
use crate::guard::{Caps, Guard, MIB, TOP};
use crate::proxy;

/// Graph's base: the only place a run's requests go, through the proxy.
pub const GRAPH: &str = "https://graph.microsoft.com/v1.0/";

/// Guard 2: a test account has used less than this…
pub const MAX_QUOTA_USED: u64 = 1 << 30;
/// …and holds fewer items than this. A real account fails at least one of the two, even if its
/// drive id were put on the allow-list by mistake.
pub const MAX_ITEMS: u64 = 1000;

pub struct Options {
    /// Graph's base, ending in `/`: [`GRAPH`], or a mock server's in the tests.
    pub upstream: Url,
    /// From `konedrivectl dev export-access-token --read-write` (guard 5).
    pub token: String,
    /// From `konedrivectl dev export-access-token`, after the account was switched back to
    /// read-only; none when `--only` picks checks that do not use it.
    pub read_only_token: Option<String>,
    /// `--graph-test-drive`.
    pub test_drive: String,
    /// The daemon's `config.toml`, for `write_test_drive_ids`.
    pub config: Config,
    pub caps: Caps,
    pub run_id: String,
    /// How long the delta check waits for OneDrive to report the run's changes.
    pub delta_wait: Duration,
    /// `--large-test-drive`: guard 2's size limits are not applied.
    pub large: bool,
    /// `--only`.
    pub only: Option<Only>,
}

/// How a run ended.
#[derive(Debug)]
pub enum Ended {
    /// A guard did not hold before anything was written: nothing was.
    Refused(String),
    /// The checks ran (all of them, or until the guard refused a request), then the cleanup.
    Ran(Report),
}

#[derive(Debug)]
pub struct Report {
    pub checks: Vec<(&'static str, Outcome)>,
    /// The guard's refusal, when it cut the run short.
    pub refused: Option<String>,
    pub requests: u32,
    pub bytes: u64,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|(_, outcome)| matches!(outcome, Outcome::Fail(_)))
    }
}

/// A few requests `DriveClient` has no call for (the drive's quota, a restore), sent through the
/// proxy like everything else.
pub struct Api {
    http: reqwest::Client,
    base: Url,
    token: String,
}

impl Api {
    pub fn new(base: Url, token: &str) -> Api {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("an HTTP client");
        Api { http, base, token: token.to_owned() }
    }

    fn url(&self, segments: &[&str]) -> Result<Url, String> {
        let mut url = self.base.clone();
        url.path_segments_mut().map_err(|()| "the proxy's base cannot take a path".to_owned())?.pop_if_empty().extend(segments);
        Ok(url)
    }

    async fn answer(request: reqwest::RequestBuilder) -> Result<(u16, Value), String> {
        let answer = request.send().await.map_err(|e| format!("cannot reach the proxy: {}", e.without_url()))?;
        let status = answer.status().as_u16();
        let body = answer.bytes().await.map_err(|e| format!("an unreadable answer: {}", e.without_url()))?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    pub async fn get(&self, segments: &[&str]) -> Result<(u16, Value), String> {
        Self::answer(self.http.get(self.url(segments)?).bearer_auth(&self.token)).await
    }

    pub async fn get_query(&self, segments: &[&str], query: &str) -> Result<(u16, Value), String> {
        let mut url = self.url(segments)?;
        url.set_query(Some(query));
        Self::answer(self.http.get(url).bearer_auth(&self.token)).await
    }

    pub async fn post(&self, segments: &[&str], body: &Value) -> Result<(u16, Value), String> {
        Self::answer(self.http.post(self.url(segments)?).bearer_auth(&self.token).json(body)).await
    }
}

/// What the preflight found.
pub struct Preflight {
    pub root: String,
    /// Where the delta check starts: the end of the preflight's listing.
    pub delta_link: String,
}

pub async fn run(options: Options) -> Ended {
    // Guard 1's first half, and guard 5's part here, before any request.
    if !DriveId::new(options.test_drive.as_str()).is_some_and(|drive| options.config.read_write_export_allowed(&drive)) {
        return Ended::Refused(format!(
            "--graph-test-drive {} is not in write_test_drive_ids in the daemon's config.toml",
            options.test_drive
        ));
    }
    if options.read_only_token.as_ref() == Some(&options.token) {
        return Ended::Refused("the read-only token is the read-write one: export it after the switch back to read-only".into());
    }
    let guard = Arc::new(Guard::new(options.caps, options.run_id.clone()));
    let proxy = match proxy::start(options.upstream.clone(), Arc::clone(&guard)).await {
        Ok(proxy) => proxy,
        Err(e) => return Ended::Refused(format!("the guard's proxy did not start: {e}")),
    };
    let client = |token: &str| DriveClient::new(proxy.base.clone(), Arc::new(StaticToken::new(token)));
    let drive = match client(&options.token) {
        Ok(drive) => drive,
        Err(e) => return Ended::Refused(format!("no Graph client: {e}")),
    };
    let read_only = match options.read_only_token.as_deref().map(client).transpose() {
        Ok(read_only) => read_only,
        Err(e) => return Ended::Refused(format!("no Graph client: {e}")),
    };
    let api = Api::new(proxy.base.clone(), &options.token);
    let read_only_api = options.read_only_token.as_deref().map(|token| Api::new(proxy.base.clone(), token));

    let preflight = match preflight(&options, &api, read_only_api.as_ref(), &drive).await {
        Ok(preflight) => preflight,
        Err(why) => return Ended::Refused(why),
    };
    if let Some(why) = guard.refused() {
        return Ended::Refused(why);
    }
    guard.arm();

    const CLEANUP: &str = "cleanup: the run folder goes to the recycle bin";
    let mut checks = Vec::new();
    let folder = match checks::bootstrap(&drive, &guard, &preflight.root, &options.run_id).await {
        Ok(folder) => {
            println!("  run folder /{TOP}/{} ({folder})", options.run_id);
            Some(folder)
        }
        Err(why) => {
            let outcome = Outcome::Fail(why);
            checks::show("the run folder is made", &outcome);
            checks.push(("the run folder is made", outcome));
            // Made, then something else went wrong: it still goes to the recycle bin.
            guard.run_folder()
        }
    };
    if let Some(folder) = folder {
        let made = checks.is_empty();
        let mut run = checks::Run::new(drive, read_only, api, Arc::clone(&guard), folder, &options, preflight.delta_link);
        if made {
            checks = match options.only {
                None => run.all().await,
                Some(Only::Placeholders) => run.placeholders().await,
                Some(Only::Notifications) => run.notifications().await,
            };
        }
        let outcome = run.cleanup().await;
        checks::show(CLEANUP, &outcome);
        checks.push((CLEANUP, outcome));
    }
    let (requests, bytes) = guard.usage();
    Ended::Ran(Report { checks, refused: guard.refused(), requests, bytes })
}

/// Guards 1 and 2, with reads only: the drive both tokens reach is the one named, and it looks
/// like a test account (unless `--large-test-drive` says it is a large one). Then the root's id,
/// which the guard needs to let the run folder be made.
async fn preflight(options: &Options, api: &Api, read_only: Option<&Api>, drive: &DriveClient) -> Result<Preflight, String> {
    let (status, me) = api.get(&["me", "drive"]).await?;
    if status != 200 {
        return Err(format!("GET /me/drive answered {status}"));
    }
    let id = me.get("id").and_then(Value::as_str).unwrap_or_default();
    if id != options.test_drive {
        return Err(format!("the read-write token reaches drive {id:?}, not --graph-test-drive {}", options.test_drive));
    }
    let used = me
        .pointer("/quota/used")
        .and_then(Value::as_u64)
        .ok_or("GET /me/drive gave no quota.used: a test account cannot be told from another")?;
    if used >= MAX_QUOTA_USED && !options.large {
        return Err(format!(
            "the drive has {:.1} MiB in use: a test account holds less than {} MiB",
            used as f64 / MIB as f64,
            MAX_QUOTA_USED / MIB
        ));
    }
    if let Some(read_only) = read_only {
        let (status, me) = read_only.get(&["me", "drive"]).await?;
        let read_only_id = me.get("id").and_then(Value::as_str).unwrap_or_default();
        if status != 200 || read_only_id != options.test_drive {
            return Err(format!(
                "the read-only token reaches drive {read_only_id:?} (GET /me/drive answered {status}), not --graph-test-drive {}",
                options.test_drive
            ));
        }
    }
    if options.large {
        // Not counted: the delta check starts from now.
        let (status, latest) = api.get_query(&["me", "drive", "root", "delta"], "token=latest").await?;
        let delta_link = latest.get("@odata.deltaLink").and_then(Value::as_str).unwrap_or_default().to_owned();
        if status != 200 || delta_link.is_empty() {
            return Err(format!("the delta feed's latest link: GET answered {status}"));
        }
        let root = drive.item("root").await.map_err(|e| format!("reading the drive's root: {e}"))?;
        println!(
            "  preflight: drive {id}, as named and allowed; {:.1} MiB in use, a large test account (--large-test-drive)",
            used as f64 / MIB as f64
        );
        return Ok(Preflight { root: root.id, delta_link });
    }
    let mut from = DeltaFrom::Start;
    let mut items: u64 = 0;
    let delta_link = loop {
        let page = drive.delta(&from).await.map_err(|e| format!("listing the drive: {e}"))?;
        items += page.items.iter().filter(|item| item.deleted.is_none()).count() as u64;
        if items >= MAX_ITEMS {
            return Err(format!("the drive lists {items} items or more: a test account holds fewer than {MAX_ITEMS}"));
        }
        match page.next {
            DeltaNext::Page(link) => from = DeltaFrom::Link(link),
            DeltaNext::Done(link) => break link,
        }
    };
    let root = drive.item("root").await.map_err(|e| format!("reading the drive's root: {e}"))?;
    println!(
        "  preflight: drive {id}, as named and allowed; {:.1} MiB in use; {items} items",
        used as f64 / MIB as f64
    );
    Ok(Preflight { root: root.id, delta_link })
}
