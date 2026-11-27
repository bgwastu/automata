//! Building and starting apps with Docker Compose.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tokio::process::Command;

use crate::config::App;
use crate::ctx::{Ctx, Release, now_ms};
use crate::docker::{self, docker};
use crate::git;

#[derive(Debug, Clone)]
pub struct Target {
    pub rev: String,
    pub commit: Option<String>,
    /// Infra repo commit the app's folder was read from. Not part of the rev: an infra change
    /// elsewhere in the repo does not redeploy this app.
    pub infra: Option<String>,
}

pub const UP: &[&str] = &["up", "-d", "--build", "--wait", "--wait-timeout", "300", "--remove-orphans"];

/// A `docker compose` command for a release, with the app's secrets in its environment
/// for `${VAR}` interpolation and nothing else from Automata's environment.
pub fn compose(name: &str, r: &Release, args: &[&str]) -> Command {
    let secrets: BTreeMap<String, String> = std::fs::read(r.dir.join("secrets.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut files = vec!["-f".into(), r.compose.display().to_string()];
    if r.dir.join("override.json").exists() {
        files.extend(["-f".into(), r.dir.join("override.json").display().to_string()]);
    }
    let mut cmd = docker(
        ["compose", "-p", name].into_iter().map(String::from).chain(files).chain(args.iter().map(|a| a.to_string())),
    );
    cmd.env_clear();
    for k in ["PATH", "HOME", "DOCKER_HOST", "DOCKER_CONFIG", "DOCKER_CERT_PATH", "DOCKER_TLS_VERIFY"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    cmd.envs(secrets);
    cmd
}

pub async fn deploy(ctx: &Ctx, app: &App, target: &Target) {
    let name = &app.name;
    ctx.log_clear(name);
    let dir = ctx.builds(name).join(&target.rev);
    let current = ctx.app(name, |s| s.current.clone());
    let live = current.as_ref().is_some_and(|c| c.dir == dir);
    let mut touched = false;
    let result = async {
        ctx.log(name, format!("deploying {}", target.rev));
        let release = prepare(ctx, app, target, &dir).await?;
        touched = true;
        docker::must(compose(name, &release, UP), |l| ctx.log(name, l)).await?;
        settled(ctx, name).await?;
        Ok::<_, anyhow::Error>(release)
    }
    .await;

    match result {
        Ok(release) => {
            ctx.app(name, |s| {
                s.current = Some(release);
                s.failed = None;
                s.error = None;
            });
            ctx.log(name, "deployed");
            prune(ctx, name).await;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            ctx.log(name, format!("failed: {msg}"));
            ctx.app(name, |s| {
                s.failed = Some(target.rev.clone());
                s.error = Some(msg);
            });
            if let Some(cur) = current.filter(|_| touched && !live) {
                ctx.log(name, format!("restoring {}", cur.rev));
                let _ = docker::run(compose(name, &cur, UP), |l| ctx.log(name, l)).await;
            }
            if !live {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
        }
    }
    ctx.save().await;
}

/// Fetch the source into the build folder and write the Compose override and secrets next to it.
async fn prepare(ctx: &Ctx, app: &App, target: &Target, dir: &Path) -> Result<Release> {
    let src = dir.join("src");
    tokio::fs::create_dir_all(dir).await?;
    let context = match (&app.source, &target.commit) {
        (Some(source), Some(commit)) => {
            ctx.log(&app.name, format!("downloading {}@{}", source.github, &commit[..7]));
            git::download(ctx, &source.github, commit, &src).await?;
            let context = normalize(&src.join(&source.path));
            if !context.starts_with(&src) {
                bail!("source.path leaves the repository");
            }
            context
        }
        _ => {
            let (from, to) = (app.dir.clone(), src.clone());
            tokio::task::spawn_blocking(move || copy_dir(&from, &to)).await??;
            src
        }
    };

    let file = compose_file(&context, app).await?;
    let order = service_order(&file)?;
    let secrets = app.env.0.clone();
    let release = Release {
        rev: target.rev.clone(),
        commit: target.commit.clone(),
        infra: target.infra.clone(),
        dir: dir.into(),
        compose: file,
        at: now_ms(),
    };

    // Let Compose resolve includes, extends and interpolation, then add our settings on top.
    tokio::fs::write(dir.join("secrets.json"), serde_json::to_vec(&secrets)?).await?;
    tokio::fs::set_permissions(dir.join("secrets.json"), std::fs::Permissions::from_mode(0o600)).await?;
    let _ = tokio::fs::remove_file(dir.join("override.json")).await;
    let resolved: Value =
        serde_json::from_str(&docker::output(compose(&app.name, &release, &["config", "--format", "json"])).await?)?;
    let services = resolved["services"].as_object().context("compose file has no services")?;
    let main = app
        .service
        .clone()
        .or_else(|| order.into_iter().find(|s| services.contains_key(s)))
        .context("compose file has no services")?;
    if !services.contains_key(&main) {
        bail!("service \"{main}\" is not in the compose file");
    }
    let o = override_for(&app.name, services, &main, &secrets);
    tokio::fs::write(dir.join("override.json"), serde_json::to_vec_pretty(&o)?).await?;
    Ok(release)
}

/// `up --wait` only waits for "running" when a service has no healthcheck, so a crash loop
/// passes. Give containers a few seconds, then fail if any restarted, are unhealthy or exited non-zero.
pub async fn settled(ctx: &Ctx, name: &str) -> Result<()> {
    ctx.log(name, "checking containers stay up");
    let before = restarts(name).await?;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let after = restarts(name).await?;
    let bad: Vec<_> = docker::containers()
        .await?
        .into_iter()
        .filter(|c| c.app == name)
        .filter(|c| {
            c.state == "restarting"
                || after.get(&c.id) > before.get(&c.id).or(Some(&0))
                || c.health.as_deref() == Some("unhealthy")
                || (c.status.starts_with("Exited (") && !c.status.starts_with("Exited (0)"))
        })
        .collect();
    let Some(first) = bad.first() else { return Ok(()) };
    let logs = docker::run(docker(["logs", "--tail", "15", &first.id]), |_| {}).await?.tail;
    let names: Vec<_> = bad.iter().map(|c| c.service.as_str()).collect();
    let logs = if logs.trim().is_empty() { String::new() } else { format!("\n{}", logs.trim()) };
    bail!("{} keeps failing after start{logs}", names.join(", "))
}

/// Restart count of each of the app's containers, by short id.
async fn restarts(name: &str) -> Result<BTreeMap<String, u64>> {
    let filter = format!("label=com.docker.compose.project={name}");
    let ids = docker::output(docker(["ps", "-aq", "--filter", &filter])).await?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let out = docker::output(docker(["inspect", "-f", "{{.Id}} {{.RestartCount}}"].into_iter().chain(ids))).await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (id, n) = l.split_once(' ')?;
            Some((id.get(..12)?.to_string(), n.parse().ok()?))
        })
        .collect())
}

pub async fn restart(ctx: &Ctx, name: &str) -> Result<()> {
    let cur = ctx.app(name, |s| s.current.clone()).context("not deployed")?;
    ctx.log_clear(name);
    docker::must(compose(name, &cur, &["restart"]), |l| ctx.log(name, l)).await
}

/// Stop and delete an app's containers. Volumes stay, so re-adding the app brings its data back.
pub async fn remove(ctx: &Ctx, name: &str) -> Result<()> {
    if let Some(cur) = ctx.state(|s| s.apps.get(name).and_then(|a| a.current.clone())) {
        docker::must(compose(name, &cur, &["down", "--remove-orphans"]), |_| {}).await?;
    }
    let _ = tokio::fs::remove_dir_all(ctx.builds(name)).await;
    ctx.state(|s| s.apps.remove(name));
    ctx.save().await;
    Ok(())
}

async fn compose_file(dir: &Path, app: &App) -> Result<PathBuf> {
    if let Some(image) = &app.image {
        let file = dir.join("automata.compose.json");
        tokio::fs::write(&file, image_compose(image, &app.volumes).to_string()).await?;
        return Ok(file);
    }
    for f in ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"] {
        if tokio::fs::try_exists(dir.join(f)).await? {
            return Ok(dir.join(f));
        }
    }
    if !tokio::fs::try_exists(dir.join("Dockerfile")).await? {
        bail!("no compose.yaml or Dockerfile found");
    }
    let file = dir.join("automata.compose.json");
    let generated =
        json!({ "services": { "app": { "build": ".", "volumes": ["data:/data"] } }, "volumes": { "data": {} } });
    tokio::fs::write(&file, generated.to_string()).await?;
    Ok(file)
}

/// A one-service compose file for `image:` apps. Named volumes are declared so Compose creates them
/// as `<app>_<name>`, which is what backups look for.
fn image_compose(image: &str, volumes: &[String]) -> Value {
    let named: Map<String, Value> = volumes
        .iter()
        .filter_map(|v| v.split(':').next())
        .filter(|from| !from.starts_with(['/', '.', '~']))
        .map(|from| (from.to_string(), json!({})))
        .collect();
    let mut svc = json!({ "image": image });
    if !volumes.is_empty() {
        svc["volumes"] = json!(volumes);
    }
    let mut out = json!({ "services": { "app": svc } });
    if !named.is_empty() {
        out["volumes"] = Value::Object(named);
    }
    out
}

/// Service names in file order. `compose config` sorts them, and the first one is the default main service.
fn service_order(file: &Path) -> Result<Vec<String>> {
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&std::fs::read_to_string(file)?)?;
    Ok(doc
        .get("services")
        .and_then(|s| s.as_mapping())
        .map(|m| m.keys().filter_map(|k| k.as_str().map(String::from)).collect())
        .unwrap_or_default())
}

/// What Automata adds to every app's compose file.
fn override_for(name: &str, services: &Map<String, Value>, main: &str, secrets: &BTreeMap<String, String>) -> Value {
    let mut out = Map::new();
    for (svc, def) in services {
        let mut o = json!({ "labels": { "automata.app": name } });
        if def.get("restart").is_none() {
            o["restart"] = json!("unless-stopped");
        }
        if def.get("logging").is_none() {
            o["logging"] = json!({ "driver": "json-file", "options": { "max-size": "10m", "max-file": "3" } });
        }
        if svc == main {
            // Compose interpolates the override too, so a literal "$" must be written "$$".
            let env: Map<String, Value> =
                secrets.iter().map(|(k, v)| (k.clone(), json!(v.replace('$', "$$")))).collect();
            o["environment"] = Value::Object(env);
            if def.get("network_mode").is_none() {
                let mut nets: Map<String, Value> = def
                    .get("networks")
                    .and_then(|n| n.as_object())
                    .map(|n| n.keys().map(|k| (k.clone(), json!({}))).collect())
                    .unwrap_or_else(|| Map::from_iter([("default".into(), json!({}))]));
                nets.insert("automata".into(), json!({ "aliases": [name] }));
                o["networks"] = Value::Object(nets);
            }
        }
        out.insert(svc.clone(), o);
    }
    json!({ "services": out, "networks": { "automata": { "external": true, "name": "automata" } } })
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        std::fs::remove_dir_all(to)?;
    }
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(".env") {
            continue;
        }
        let dest = to.join(entry.path().strip_prefix(from)?);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Remove build folders other than the running one.
async fn prune(ctx: &Ctx, name: &str) {
    let keep = ctx.app(name, |s| s.current.as_ref().map(|c| c.dir.clone()));
    let Ok(mut dirs) = tokio::fs::read_dir(ctx.builds(name)).await else { return };
    while let Ok(Some(d)) = dirs.next_entry().await {
        if Some(d.path()) != keep {
            let _ = tokio::fs::remove_dir_all(d.path()).await;
        }
    }
}

#[cfg(test)]
mod image_tests {
    use super::image_compose;
    use serde_json::json;

    #[test]
    fn generated_compose() {
        assert_eq!(image_compose("nginx", &[]), json!({ "services": { "app": { "image": "nginx" } } }));
        let vols = ["data:/app/data".to_string(), "/srv/x:/x:ro".to_string()];
        assert_eq!(
            image_compose("ghcr.io/a/b:latest", &vols),
            json!({
                "services": { "app": { "image": "ghcr.io/a/b:latest", "volumes": ["data:/app/data", "/srv/x:/x:ro"] } },
                "volumes": { "data": {} },
            })
        );
    }
}
