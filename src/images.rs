//! Keeping `:latest` images current. Any other tag or a digest is a pin and is left alone.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::ctx::{Ctx, Outcome, now_ms};
use crate::deploy::{compose, settled};
use crate::docker::{self, docker};

/// True when `image` follows the latest tag, written or implied.
pub fn is_latest(image: &str) -> bool {
    if image.contains('@') {
        return false;
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    last.split_once(':').is_none_or(|(_, tag)| tag == "latest")
}

/// Pull new versions of the app's `:latest` images and recreate the services that use them.
/// If a service fails to stay up on the new image, put the old image back.
pub async fn update(ctx: &Ctx, name: &str) -> Result<()> {
    let Some(cur) = ctx.app(name, |s| s.current.clone()) else { return Ok(()) };
    let resolved: Value =
        serde_json::from_str(&docker::output(compose(name, &cur, &["config", "--format", "json"])).await?)?;
    let images: BTreeMap<String, String> = resolved["services"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, def)| def.get("build").is_none())
        .filter_map(|(svc, def)| Some((svc.clone(), def.get("image")?.as_str()?.to_string())))
        .filter(|(_, image)| is_latest(image))
        .collect();
    if images.is_empty() {
        return Ok(());
    }

    let services: Vec<&str> = images.keys().map(String::as_str).collect();
    let pull: Vec<&str> = ["pull", "--quiet"].into_iter().chain(services.iter().copied()).collect();
    docker::must(compose(name, &cur, &pull), |_| {}).await?;

    let running = running_images(name).await?;
    let bad = ctx.app(name, |s| s.bad_images.clone());
    let mut changed = vec![];
    for (svc, image) in &images {
        let local = docker::output(docker(["image", "inspect", "-f", "{{.Id}}", image])).await?.trim().to_string();
        let old = running.get(svc);
        if old.is_some_and(|o| *o != local) && bad.get(svc) != Some(&local) {
            changed.push((svc.clone(), image.clone(), old.cloned().unwrap(), local));
        }
    }
    if changed.is_empty() {
        ctx.app(name, |s| s.images = Some(Outcome { at: now_ms(), ok: true, message: None }));
        ctx.save().await;
        return Ok(());
    }

    ctx.log_clear(name);
    let list: Vec<String> = changed.iter().map(|(svc, image, ..)| format!("{svc} ({image})")).collect();
    ctx.log(name, format!("new image for {}", list.join(", ")));
    let svcs: Vec<&str> = changed.iter().map(|(svc, ..)| svc.as_str()).collect();
    let up: Vec<&str> =
        ["up", "-d", "--wait", "--wait-timeout", "300", "--no-build"].into_iter().chain(svcs.iter().copied()).collect();
    let result = async {
        docker::must(compose(name, &cur, &up), |l| ctx.log(name, l)).await?;
        settled(ctx, name).await
    }
    .await;

    let outcome = match result {
        Ok(()) => {
            ctx.log(name, "updated");
            ctx.app(name, |s| {
                for (svc, ..) in &changed {
                    s.bad_images.remove(svc);
                }
            });
            Outcome { at: now_ms(), ok: true, message: Some(format!("updated {}", svcs.join(", "))) }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            ctx.log(name, format!("failed: {msg}"));
            ctx.log(name, "putting the previous images back");
            for (_, image, old, _) in &changed {
                let _ = docker::output(docker(["tag", old.as_str(), image.as_str()])).await;
            }
            let _ = docker::run(compose(name, &cur, &up), |l| ctx.log(name, l)).await;
            ctx.app(name, |s| {
                for (svc, _, _, new) in &changed {
                    s.bad_images.insert(svc.clone(), new.clone());
                }
            });
            Outcome { at: now_ms(), ok: false, message: Some(msg) }
        }
    };
    ctx.app(name, |s| s.images = Some(outcome));
    ctx.save().await;
    Ok(())
}

/// Image id each service's container runs, by service name.
async fn running_images(name: &str) -> Result<BTreeMap<String, String>> {
    let filter = format!("label=com.docker.compose.project={name}");
    let ids = docker::output(docker(["ps", "-q", "--filter", &filter])).await?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let format = r#"{{index .Config.Labels "com.docker.compose.service"}} {{.Image}}"#;
    let out = docker::output(docker(["inspect", "-f", format].into_iter().chain(ids))).await?;
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split_once(' ').map(|(s, i)| (s.to_string(), i.to_string())).context("unexpected inspect output"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_latest;

    #[test]
    fn latest_detection() {
        assert!(is_latest("nginx"));
        assert!(is_latest("nginx:latest"));
        assert!(is_latest("ghcr.io/a/b:latest"));
        assert!(is_latest("localhost:5000/app"));
        assert!(!is_latest("localhost:5000/app:1.2"));
        assert!(!is_latest("nginx:1.27"));
        assert!(!is_latest("nginx@sha256:abc"));
    }
}
