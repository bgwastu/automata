//! Replacing the running binary with the newest GitHub release.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub struct Update {
    pub version: semver::Version,
    binary: String,
    checksum: String,
}

/// The newest release, if it is newer than this binary and has a build for this machine.
pub async fn check(http: &reqwest::Client, repo: &str, token: Option<&str>) -> Result<Option<Update>> {
    let mut req =
        http.get(format!("https://api.github.com/repos/{repo}/releases/latest")).timeout(Duration::from_secs(20));
    // Signed-in requests get a much higher rate limit.
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await?;
    if res.status() == 404 {
        return Ok(None);
    }
    let release: Release = res.error_for_status()?.json().await?;
    let version =
        semver::Version::parse(release.tag_name.trim_start_matches('v')).context("release tag is not a version")?;
    if version <= semver::Version::parse(VERSION)? {
        return Ok(None);
    }
    let name = format!("automata-{}-linux", std::env::consts::ARCH);
    let url = |n: &str| release.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let (Some(binary), Some(checksum)) = (url(&name), url(&format!("{name}.sha256"))) else {
        bail!("release {version} has no {name}");
    };
    Ok(Some(Update { version, binary, checksum }))
}

/// Download, verify and swap in the new binary. Returns its path.
pub async fn install(http: &reqwest::Client, update: &Update) -> Result<PathBuf> {
    let get = |url: String| async move {
        let res = http.get(url).timeout(Duration::from_secs(300)).send().await?.error_for_status()?;
        anyhow::Ok(res.bytes().await?)
    };
    let sum = String::from_utf8(get(update.checksum.clone()).await?.to_vec())?;
    let want = sum.split_whitespace().next().context("empty checksum file")?.to_lowercase();
    let bytes = get(update.binary.clone()).await?;
    if hex::encode(Sha256::digest(&bytes)) != want {
        bail!("checksum mismatch for {}", update.binary);
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    let tmp = exe.with_file_name(".automata.new");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).await?;
    let out = tokio::process::Command::new(&tmp).arg("--version").output().await;
    let runs = out
        .as_ref()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&update.version.to_string()));
    if !runs {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("the new binary does not run on this machine");
    }
    tokio::fs::rename(&tmp, &exe).await?;
    Ok(exe)
}

/// The scheduled check. Installs the update between tasks and restarts in place.
pub async fn run(ctx: &Ctx) {
    let found = check(&ctx.http, &ctx.env.release_repo, ctx.env.github_token.as_deref()).await;
    let update = {
        let mut s = ctx.self_status.lock().unwrap();
        match found {
            Ok(u) => {
                s.latest = u.as_ref().map(|u| u.version.to_string());
                s.error = None;
                u
            }
            Err(e) => {
                s.error = Some(format!("{e:#}"));
                None
            }
        }
    };
    ctx.changed();
    let on = ctx.config().is_none_or(|c| c.self_update);
    let Some(update) = update.filter(|_| on) else { return };

    ctx.queue("automata", "self-update", async {
        tracing::info!("updating to {}", update.version);
        match install(&ctx.http, &update).await {
            Ok(exe) => restart(&exe),
            Err(e) => {
                tracing::error!("self-update: {e:#}");
                ctx.self_status.lock().unwrap().error = Some(format!("update to {}: {e:#}", update.version));
            }
        }
    })
    .await;
}

/// Replace this process with the new binary. Same pid, so systemd keeps tracking it.
fn restart(exe: &Path) {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
    tracing::error!("restarting after update: {err}");
    std::process::exit(1);
}
