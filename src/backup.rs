//! Volume backups with restic, run in throwaway containers at the lowest CPU and IO priority.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::{App, BackupRepo};
use crate::ctx::{Ctx, Outcome, now_ms};
use crate::deploy::compose;
use crate::docker::{self, docker};

const IMAGE: &str = "restic/restic:0.18.1";

async fn restic(ctx: &Ctx, repo: &BackupRepo, mounts: &[String], args: &[&str]) -> Result<Command> {
    let config = ctx.config().context("the infra repo is not loaded")?;
    let r2 = crate::tunnel::r2(ctx, &config, &repo.bucket).await?;
    let mut a: Vec<String> = ["run", "--rm", "--cpu-shares", "128", "-v", "automata-restic-cache:/cache"]
        .into_iter()
        .map(String::from)
        .collect();
    for k in ["RESTIC_REPOSITORY", "RESTIC_PASSWORD", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
        a.extend(["-e".into(), k.into()]);
    }
    for m in mounts {
        a.extend(["-v".into(), m.clone()]);
    }
    a.extend(["--entrypoint", "nice", IMAGE, "-n", "19", "restic", "--cache-dir", "/cache"].map(String::from));
    a.extend(args.iter().map(|s| s.to_string()));
    let mut cmd = docker(a);
    cmd.env("RESTIC_REPOSITORY", &r2.repository);
    cmd.env("RESTIC_PASSWORD", crate::dotenv::derive("restic")?);
    cmd.env("AWS_ACCESS_KEY_ID", &r2.key_id);
    cmd.env("AWS_SECRET_ACCESS_KEY", &r2.secret);
    Ok(cmd)
}

pub async fn backup(ctx: &Ctx, repo: &BackupRepo, app: &App) {
    let name = &app.name;
    ctx.log_clear(name);
    let result = async {
        let vols = docker::volumes(name).await?;
        if vols.is_empty() {
            bail!("app has no volumes to back up");
        }
        if let Some(b) = &app.backup
            && let (Some(before), Some(service)) = (&b.before, &b.service)
        {
            ctx.log(name, format!("running before hook in {service}"));
            let filters = [
                format!("label=com.docker.compose.project={name}"),
                format!("label=com.docker.compose.service={service}"),
            ];
            let ids = docker::output(docker(["ps", "-q", "--filter", &filters[0], "--filter", &filters[1]])).await?;
            let id = ids.split_whitespace().next().with_context(|| format!("service {service} is not running"))?;
            docker::must(docker(["exec", id, "sh", "-c", before]), |l| ctx.log(name, l))
                .await
                .context("before hook")?;
        }
        // restic exits 10 when the repository does not exist yet.
        let probe = restic(ctx, repo, &[], &["cat", "config"])
            .await?
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        if probe.code() == Some(10) {
            ctx.log(name, "initializing repository");
            docker::must(restic(ctx, repo, &[], &["init"]).await?, |l| ctx.log(name, l)).await?;
        }
        let mounts: Vec<String> = vols.iter().map(|v| format!("{v}:/volumes/{v}:ro")).collect();
        let limit = repo.limit_upload.map(|l| l.to_string());
        let mut args = vec!["backup", "/volumes", "--host", name, "--tag", "automata"];
        if let Some(l) = &limit {
            args.extend(["--limit-upload", l]);
        }
        docker::must(restic(ctx, repo, &mounts, &args).await?, |l| ctx.log(name, l)).await?;
        let k = &repo.keep;
        let (d, w, m) = (k.daily.to_string(), k.weekly.to_string(), k.monthly.to_string());
        let forget = ["forget", "--host", name, "--keep-daily", &d, "--keep-weekly", &w, "--keep-monthly", &m];
        docker::must(restic(ctx, repo, &[], &forget).await?, |l| ctx.log(name, l)).await
    }
    .await;
    let outcome = match result {
        Ok(()) => Outcome { at: now_ms(), ok: true, message: None },
        Err(e) => {
            ctx.log(name, format!("failed: {e:#}"));
            Outcome { at: now_ms(), ok: false, message: Some(format!("{e:#}")) }
        }
    };
    ctx.app(name, |s| s.backup = Some(outcome));
    ctx.save().await;
}

/// Weekly prune and monthly partial read-back check of the whole repository.
pub async fn upkeep(ctx: &Ctx, repo: &BackupRepo, args: &[&str]) {
    let run = async { docker::must(restic(ctx, repo, &[], args).await?, |_| {}).await };
    if let Err(e) = run.await {
        tracing::error!("restic {}: {e:#}", args[0]);
    }
    ctx.changed();
}

#[derive(Serialize)]
pub struct Snapshot {
    id: String,
    time: String,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct RawSnapshot {
    short_id: String,
    time: String,
    summary: Option<Summary>,
}

#[derive(Deserialize)]
struct Summary {
    total_bytes_processed: Option<u64>,
}

pub async fn snapshots(ctx: &Ctx, repo: &BackupRepo, name: &str) -> Result<Vec<Snapshot>> {
    let out = restic(ctx, repo, &[], &["snapshots", "--host", name, "--json"]).await?.output().await?;
    // Exit code 10: no repository yet, so no snapshots.
    if out.status.code() == Some(10) {
        return Ok(vec![]);
    }
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let raw: Vec<RawSnapshot> = serde_json::from_slice(&out.stdout)?;
    Ok(raw
        .into_iter()
        .rev()
        .map(|s| Snapshot { id: s.short_id, time: s.time, size: s.summary.and_then(|s| s.total_bytes_processed) })
        .collect())
}

/// Stop the app, put its volumes back to `snapshot`, start it again.
pub async fn restore(ctx: &Ctx, repo: &BackupRepo, name: &str, snapshot: &str) -> Result<()> {
    if snapshot.len() < 8 || !snapshot.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad snapshot id");
    }
    let cur = ctx.app(name, |s| s.current.clone()).context("app is not deployed")?;
    ctx.log_clear(name);
    let vols = docker::volumes(name).await?;
    let mounts: Vec<String> = vols.iter().map(|v| format!("{v}:/restore/volumes/{v}")).collect();
    // Built before stopping, so a credentials problem leaves the app running.
    let cmd = restic(ctx, repo, &mounts, &["restore", snapshot, "--target", "/restore", "--delete"]).await?;
    ctx.log(name, format!("stopping {name}"));
    docker::must(compose(name, &cur, &["stop"]), |l| ctx.log(name, l)).await?;
    let restored = docker::must(cmd, |l| ctx.log(name, l)).await;
    docker::must(compose(name, &cur, &["start"]), |l| ctx.log(name, l)).await?;
    restored?;
    ctx.log(name, "restored");
    Ok(())
}
