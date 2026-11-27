//! Thin wrappers over the docker CLI, which Compose needs anyway.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

pub fn docker<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = Command::new("docker");
    // Compose files can interpolate the environment, so keep Automata's own secrets out of it.
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true).env_remove(crate::secret::VAR).env_remove("GITHUB_TOKEN");
    cmd
}

pub struct Output {
    pub ok: bool,
    /// The last 20 lines of output, for error messages.
    pub tail: String,
}

/// Run a command, passing each line of stdout and stderr to `on_line`.
pub async fn run(mut cmd: Command, mut on_line: impl FnMut(&str)) -> Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("starting docker")?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    forward(child.stdout.take().unwrap(), tx.clone());
    forward(child.stderr.take().unwrap(), tx);
    let mut tail = VecDeque::new();
    while let Some(line) = rx.recv().await {
        on_line(&line);
        if tail.len() == 20 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let status = child.wait().await?;
    Ok(Output { ok: status.success(), tail: Vec::from(tail).join("\n") })
}

/// Run a command and fail with its output if it exits non-zero.
pub async fn must(cmd: Command, on_line: impl FnMut(&str)) -> Result<()> {
    let out = run(cmd, on_line).await?;
    if !out.ok {
        bail!("{}", if out.tail.is_empty() { "command failed".into() } else { out.tail });
    }
    Ok(())
}

/// Run a command and return its stdout.
pub async fn output(mut cmd: Command) -> Result<String> {
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await.context("starting docker")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn forward(stream: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub app: String,
    pub service: String,
    pub name: String,
    pub state: String,
    pub health: Option<String>,
    pub status: String,
    pub image: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsLine {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    image: String,
    state: String,
    status: String,
    labels: String,
}

/// Every container Automata started, running or not.
pub async fn containers() -> Result<Vec<Container>> {
    let out = output(docker(["ps", "-a", "--filter", "label=automata.app", "--format", "{{json .}}"])).await?;
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let c: PsLine = serde_json::from_str(l)?;
            let labels: HashMap<&str, &str> = c.labels.split(',').filter_map(|kv| kv.split_once('=')).collect();
            let health = ["unhealthy", "healthy", "health: starting"]
                .into_iter()
                .find(|h| c.status.contains(&format!("({h})")))
                .map(|h| h.trim_start_matches("health: ").to_string());
            Ok(Container {
                app: labels.get("automata.app").unwrap_or(&"").to_string(),
                service: labels.get("com.docker.compose.service").map_or(c.names.clone(), |s| s.to_string()),
                name: c.names,
                id: c.id,
                state: c.state,
                health,
                status: c.status,
                image: c.image,
            })
        })
        .collect()
}

/// Resolve a dashboard container ID to an immutable full ID, after checking its app label.
pub async fn app_container_id(app: &str, id: &str) -> Option<String> {
    if !matches!(id.len(), 12 | 64) || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let record = output(docker([
        "inspect",
        "--type=container",
        "--format",
        "{{.Id}} {{index .Config.Labels \"automata.app\"}}",
        id,
    ]))
    .await
    .ok()?;
    let (full_id, owner) = record.trim_end().split_once(' ')?;
    (owner == app
        && full_id.len() == 64
        && full_id.starts_with(&id.to_ascii_lowercase())
        && full_id.bytes().all(|c| c.is_ascii_hexdigit()))
    .then(|| full_id.into())
}

/// Create the shared network if needed and return its gateway address on the host.
pub async fn ensure_network(name: &str) -> Result<String> {
    let inspect = || docker(["network", "inspect", name, "-f", "{{range .IPAM.Config}}{{.Gateway}} {{end}}"]);
    if output(inspect()).await.is_err() {
        output(docker(["network", "create", "-o", &format!("com.docker.network.bridge.name={name}0"), name])).await?;
    }
    let out = output(inspect()).await?;
    out.split_whitespace().find(|ip| ip.contains('.')).map(str::to_string).context("network has no IPv4 gateway")
}

/// Named volumes Compose created for a project.
pub async fn volumes(project: &str) -> Result<Vec<String>> {
    let filter = format!("label=com.docker.compose.project={project}");
    let out = output(docker(["volume", "ls", "-q", "--filter", &filter])).await?;
    Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
}
