//! Making the host match the infra repo.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{self, App, Config};
use crate::ctx::{Ctx, now_ms};
use crate::deploy::{self, Target};
use crate::docker::{self, docker};
use crate::sched::Job;
use crate::tunnel::{self, Route};
use crate::{git, selfupdate};

pub const NETWORK: &str = "automata";
/// How often to check the infra repo and app branches.
const INTERVAL: Duration = Duration::from_secs(30);

pub async fn run(ctx: Arc<Ctx>) {
    loop {
        once(&ctx).await;
        tokio::select! {
            _ = ctx.wake.notified() => {}
            _ = tokio::time::sleep(INTERVAL) => {}
        }
    }
}

async fn once(ctx: &Ctx) {
    let previous = ctx.config();
    let config = match sync_infra(ctx).await {
        Ok(dir) => match config::load(&dir) {
            Ok(c) => {
                let c = Arc::new(c);
                *ctx.config.write().unwrap() = Some(c.clone());
                ctx.infra.lock().unwrap().error = None;
                Some(c)
            }
            Err(e) => {
                ctx.infra.lock().unwrap().error = Some(format!("{e:#}"));
                previous.clone()
            }
        },
        Err(e) => {
            ctx.infra.lock().unwrap().error = Some(format!("{e:#}"));
            previous.clone()
        }
    };
    ctx.infra.lock().unwrap().synced_at = Some(now_ms());
    ctx.changed();
    let Some(config) = config else { return };

    let gateway = match docker::ensure_network(NETWORK).await {
        Ok(g) => g,
        Err(e) => {
            ctx.infra.lock().unwrap().error = Some(format!("docker network: {e:#}"));
            return;
        }
    };
    let tunnel = async {
        let api = tunnel::Api::new(ctx, &config)?;
        tunnel::ensure(ctx, &api, &config, &gateway).await?;
        tunnel::sync(ctx, &api, &routes(ctx, &config, previous.as_deref())).await
    };
    *ctx.tunnel_error.lock().unwrap() = tunnel.await.err().map(|e| format!("{e:#}"));
    crate::access::refresh(ctx, &config).await;
    ctx.scheduler.set(jobs(&config));
    ctx.changed();

    for app in &config.apps {
        let target = match target(ctx, app).await {
            Ok(t) => {
                ctx.problems.lock().unwrap().remove(&app.name);
                t
            }
            Err(e) => {
                ctx.problems.lock().unwrap().insert(app.name.clone(), format!("{e:#}"));
                continue;
            }
        };
        let (rev, failed) = ctx.app(&app.name, |s| (s.current.as_ref().map(|c| c.rev.clone()), s.failed.clone()));
        if rev.as_ref() == Some(&target.rev) || failed.as_ref() == Some(&target.rev) || ctx.busy(&app.name).is_some() {
            continue;
        }
        ctx.queue(&app.name, "deploy", deploy::deploy(ctx, app, &target)).await;
    }

    // Apps whose folder is gone. An app that failed to load keeps running as it was.
    let gone: Vec<String> = ctx.state(|s| {
        s.apps
            .keys()
            .filter(|n| !config.apps.iter().any(|a| &a.name == *n) && !config.invalid.contains_key(*n))
            .cloned()
            .collect()
    });
    for name in gone {
        if let Err(e) = ctx.queue(&name, "remove", deploy::remove(ctx, &name)).await {
            tracing::error!("removing {name}: {e:#}");
        }
    }
}

/// Fetch the infra repo, or use it in place when it is a local folder.
async fn sync_infra(ctx: &Ctx) -> Result<PathBuf> {
    let spec = &ctx.env.infra;
    if spec.starts_with('/') || spec.starts_with('.') {
        return Ok(PathBuf::from(spec));
    }
    let (repo, branch) = spec.split_once('#').unwrap_or((spec, "main"));
    let commit = git::head(ctx, repo, branch).await?;
    let root = ctx.data().join("infra");
    let dir = root.join(&commit);
    if !tokio::fs::try_exists(dir.join(".automata-ok")).await? {
        git::download(ctx, repo, &commit, &dir).await.context("infra repo")?;
        tokio::fs::write(dir.join(".automata-ok"), "").await?;
    }
    if let Ok(mut entries) = tokio::fs::read_dir(&root).await {
        while let Ok(Some(e)) = entries.next_entry().await {
            if e.path() != dir {
                let _ = tokio::fs::remove_dir_all(e.path()).await;
            }
        }
    }
    let mut infra = ctx.infra.lock().unwrap();
    infra.commit = Some(commit);
    infra.dir = Some(dir.clone());
    Ok(dir)
}

/// What should be running: the infra folder's hash, plus the source commit when the app has one.
async fn target(ctx: &Ctx, app: &App) -> Result<Target> {
    let infra = ctx.infra.lock().unwrap().commit.clone();
    let Some(source) = &app.source else {
        return Ok(Target { rev: app.hash[..12].into(), commit: None, infra });
    };
    let commit = match &source.commit {
        Some(c) => c.clone(),
        None => git::head(ctx, &source.github, &source.branch).await?,
    };
    Ok(Target { rev: format!("{}-{}", &commit[..7], &app.hash[..7]), commit: Some(commit), infra })
}

fn routes(ctx: &Ctx, config: &Config, previous: Option<&Config>) -> Vec<Route> {
    // Automata's own domain goes to Automata, which serves the dashboard and proxies app paths.
    let mut out: Vec<Route> = config
        .domain
        .iter()
        .map(|d| Route { hostname: d.clone(), service: format!("http://automata-host:{}", ctx.env.port), dns: true })
        .collect();
    // An app whose automata.yml stopped parsing keeps the routes it had.
    let kept = previous.into_iter().flat_map(|p| &p.apps).filter(|a| config.invalid.contains_key(&a.name));
    for app in config.apps.iter().chain(kept) {
        for d in &app.domains {
            out.push(Route { hostname: d.clone(), service: format!("http://{}:{}", app.name, app.port), dns: false });
        }
    }
    // Cloudflare matches ingress rules in order, so wildcards go after exact names.
    out.sort_by_key(|r| r.hostname.starts_with("*."));
    out
}

fn jobs(config: &Config) -> Vec<(Job, String)> {
    let mut jobs = vec![(Job::Images, config.image_check.clone()), (Job::SelfUpdate, "17 */6 * * *".into())];
    for app in &config.apps {
        if let Some(b) = &app.backup {
            jobs.push((Job::Backup(app.name.clone()), b.schedule.clone()));
        }
    }
    jobs
}

/// Dashboards refresh when containers start, stop or change health.
pub async fn watch_docker(ctx: Arc<Ctx>) {
    loop {
        let cmd = docker([
            "events",
            "--filter",
            "label=automata.app",
            "--filter",
            "type=container",
            "--format",
            "{{.Status}}",
        ]);
        let _ = docker::run(cmd, |_| ctx.changed()).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Run a scheduled job.
pub async fn job(ctx: &Ctx, job: Job) {
    let Some(config) = ctx.config() else { return };
    match job {
        Job::Backup(name) => {
            let (Some(repo), Some(app)) = (&config.backup, config.apps.iter().find(|a| a.name == name)) else { return };
            if ctx.app(&name, |s| s.current.is_some()) {
                ctx.queue(&name, "backup", crate::backup::backup(ctx, repo, app)).await;
            }
        }
        Job::Images => {
            for app in &config.apps {
                if ctx.app(&app.name, |s| s.current.is_none()) || ctx.busy(&app.name).is_some() {
                    continue;
                }
                let res = ctx.queue(&app.name, "image check", crate::images::update(ctx, &app.name)).await;
                if let Err(e) = res {
                    let outcome = crate::ctx::Outcome { at: now_ms(), ok: false, message: Some(format!("{e:#}")) };
                    ctx.app(&app.name, |s| s.images = Some(outcome));
                    ctx.save().await;
                }
            }
        }
        Job::SelfUpdate => selfupdate::run(ctx).await,
    }
}
