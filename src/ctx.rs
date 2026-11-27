//! Shared state: what is saved to disk, plus live status for the dashboard.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, watch};

use crate::config::{Config, Env};
use crate::sched::Scheduler;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub rev: String,
    pub commit: Option<String>,
    /// Infra repo commit the release was deployed from.
    #[serde(default)]
    pub infra: Option<String>,
    /// Build folder with the source, `override.json` and `secrets.json`.
    pub dir: PathBuf,
    pub compose: PathBuf,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    pub current: Option<Release>,
    /// A rev that failed to deploy. Reconcile skips it until the rev changes or someone presses sync.
    pub failed: Option<String>,
    pub error: Option<String>,
    pub backup: Option<Outcome>,
    pub images: Option<Outcome>,
    /// Image ids that failed after an update, by service, so the next check does not retry them.
    pub bad_images: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub at: i64,
    pub ok: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub account: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DnsRecord {
    pub zone: String,
    pub id: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub apps: BTreeMap<String, AppState>,
    pub infra_applied: Option<AppliedInfra>,
    pub tunnel: Option<Tunnel>,
    /// DNS records Automata created, so it can delete the ones no longer used.
    pub dns: Vec<DnsRecord>,
    /// Hash of the ingress and DNS set last pushed to Cloudflare.
    pub routes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedInfra {
    pub commit: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Infra {
    pub commit: Option<String>,
    pub error: Option<String>,
    pub synced_at: Option<i64>,
    pub syncing: bool,
    pub requested: u64,
    pub completed: u64,
    pub result: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SelfStatus {
    pub version: &'static str,
    pub latest: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum LogEvent {
    Line(String),
    Clear,
}

struct Log {
    lines: VecDeque<String>,
    tx: broadcast::Sender<LogEvent>,
}

pub struct Ctx {
    pub env: Env,
    pub http: reqwest::Client,
    state: Mutex<State>,
    state_path: PathBuf,
    write: tokio::sync::Mutex<()>,
    pub config: RwLock<Option<Arc<Config>>>,
    pub infra: Mutex<Infra>,
    pub tunnel_error: Mutex<Option<String>>,
    /// S3 credentials for R2, derived from the Cloudflare token it was made from.
    pub r2: tokio::sync::Mutex<Option<(String, crate::tunnel::R2)>>,
    pub self_status: Mutex<SelfStatus>,
    pub access: crate::access::Access,
    /// Problems that are not a failed deploy, like an unreachable source repo.
    pub problems: Mutex<HashMap<String, String>>,
    busy: Mutex<HashMap<String, String>>,
    queue: tokio::sync::Mutex<()>,
    logs: Mutex<HashMap<String, Log>>,
    pub changed: watch::Sender<u64>,
    pub wake: Notify,
    pub scheduler: Scheduler,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Ctx {
    pub fn new(env: Env) -> Result<Arc<Ctx>> {
        std::fs::create_dir_all(&env.data)?;
        let state_path = env.data.join("state.json");
        let state = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(_) => State::default(),
        };
        Ok(Arc::new(Ctx {
            http: reqwest::Client::builder()
                .user_agent(concat!("automata/", env!("CARGO_PKG_VERSION")))
                // Servers drop idle keep-alive connections after a while. Dropping ours first means
                // a sync every few minutes does not reuse one that is already closed.
                .pool_idle_timeout(Duration::from_secs(20))
                .build()?,
            env,
            state: Mutex::new(state),
            state_path,
            write: tokio::sync::Mutex::new(()),
            config: RwLock::new(None),
            infra: Mutex::default(),
            tunnel_error: Mutex::default(),
            r2: tokio::sync::Mutex::default(),
            self_status: Mutex::new(SelfStatus { version: env!("CARGO_PKG_VERSION"), ..Default::default() }),
            access: Default::default(),
            problems: Mutex::default(),
            busy: Mutex::default(),
            queue: tokio::sync::Mutex::new(()),
            logs: Mutex::default(),
            changed: watch::channel(0).0,
            wake: Notify::new(),
            scheduler: Scheduler::default(),
        }))
    }

    pub fn config(&self) -> Option<Arc<Config>> {
        self.config.read().unwrap().clone()
    }

    /// Read or change the saved state. Call `save` after changing it.
    pub fn state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn app<R>(&self, name: &str, f: impl FnOnce(&mut AppState) -> R) -> R {
        self.state(|s| f(s.apps.entry(name.into()).or_default()))
    }

    pub async fn save(&self) {
        let _w = self.write.lock().await;
        let json = self.state(|s| serde_json::to_vec_pretty(s)).expect("state serializes");
        let tmp = self.state_path.with_extension("json.tmp");
        let res = async {
            tokio::fs::write(&tmp, json).await?;
            tokio::fs::rename(&tmp, &self.state_path).await
        };
        if let Err(e) = res.await {
            tracing::error!("saving state: {e}");
        }
        self.changed();
    }

    /// Tell dashboards something changed.
    pub fn changed(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn reconcile(&self) {
        self.wake.notify_one();
    }

    pub fn busy(&self, name: &str) -> Option<String> {
        self.busy.lock().unwrap().get(name).cloned()
    }

    fn set_busy(&self, name: &str, label: Option<String>) {
        let mut busy = self.busy.lock().unwrap();
        match label {
            Some(l) => busy.insert(name.into(), l),
            None => busy.remove(name),
        };
        drop(busy);
        self.changed();
    }

    /// Run tasks one at a time, so builds and backups never compete on a small host.
    pub async fn queue<T>(&self, name: &str, label: &str, task: impl Future<Output = T>) -> T {
        self.set_busy(name, Some(format!("{label} queued")));
        let _turn = self.queue.lock().await;
        self.set_busy(name, Some(label.into()));
        let out = task.await;
        self.set_busy(name, None);
        out
    }

    pub fn log(&self, name: &str, line: impl Into<String>) {
        let line = line.into();
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(name.into()).or_insert_with(new_log);
        if log.lines.len() >= 2000 {
            log.lines.pop_front();
        }
        log.lines.push_back(line.clone());
        let _ = log.tx.send(LogEvent::Line(line));
    }

    pub fn log_clear(&self, name: &str) {
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(name.into()).or_insert_with(new_log);
        log.lines.clear();
        let _ = log.tx.send(LogEvent::Clear);
    }

    /// The lines so far, and a receiver for what comes next.
    pub fn log_follow(&self, name: &str) -> (Vec<String>, broadcast::Receiver<LogEvent>) {
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(name.into()).or_insert_with(new_log);
        (log.lines.iter().cloned().collect(), log.tx.subscribe())
    }

    pub fn builds(&self, name: &str) -> PathBuf {
        self.env.data.join("builds").join(name)
    }

    pub fn data(&self) -> &Path {
        &self.env.data
    }
}

fn new_log() -> Log {
    Log { lines: VecDeque::new(), tx: broadcast::channel(512).0 }
}
