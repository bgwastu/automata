//! Reading GitHub repos without git: ref lookup over smart HTTP, source over the tarball API.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::ctx::Ctx;

/// Commit sha at the tip of `branch`.
pub async fn head(ctx: &Ctx, repo: &str, branch: &str) -> Result<String> {
    let mut req = ctx
        .http
        .get(format!("https://github.com/{repo}.git/info/refs?service=git-upload-pack"))
        .timeout(Duration::from_secs(15));
    if let Some(t) = &ctx.env.github_token {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
        req = req.header("authorization", format!("Basic {basic}"));
    }
    let res = send(req).await.with_context(|| format!("github {repo}"))?;
    match res.status().as_u16() {
        200 => {}
        401 | 403 | 404 => bail!("github {repo}: not found or no access"),
        s => bail!("github {repo}: HTTP {s}"),
    }
    let body = res.text().await?;
    let want = format!("refs/heads/{branch}");
    // Lines look like "003f<sha> refs/heads/main\0caps", after a pkt-line length prefix.
    body.lines()
        .filter_map(|l| {
            let space = l.find(' ')?;
            let sha = l.get(space.checked_sub(40)?..space)?;
            let name = l[space + 1..].split(['\0', ' ']).next()?;
            (name == want && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sha.to_string())
        })
        .next()
        .with_context(|| format!("github {repo}: no branch {branch}"))
}

/// Download `repo` at `commit` and unpack it into `dest`.
pub async fn download(ctx: &Ctx, repo: &str, commit: &str, dest: &Path) -> Result<()> {
    let mut req =
        ctx.http.get(format!("https://api.github.com/repos/{repo}/tarball/{commit}")).timeout(Duration::from_secs(300));
    if let Some(t) = &ctx.env.github_token {
        req = req.bearer_auth(t);
    }
    let res = send(req).await.with_context(|| format!("download {repo}"))?;
    if !res.status().is_success() {
        bail!("download {repo}@{}: HTTP {}", &commit[..7.min(commit.len())], res.status());
    }
    let bytes = res.bytes().await?;
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || unpack(&bytes, &dest)).await?
}

/// Send, retrying twice when the request never got an answer. GitHub sometimes closes a pooled
/// connection just as it is reused, which fails with "connection closed before message completed".
async fn send(req: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        let try_req = req.try_clone().expect("GET requests have no streaming body");
        match try_req.send().await {
            Err(e) if attempt < 2 && (e.is_connect() || e.is_request() || e.is_timeout()) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            r => return r,
        }
    }
}

fn unpack(gz: &[u8], dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if matches!(entry.header().entry_type(), tar::EntryType::XGlobalHeader | tar::EntryType::XHeader) {
            continue;
        }
        // GitHub wraps everything in "<owner>-<repo>-<sha>/".
        let path: PathBuf = entry.path()?.components().skip(1).collect();
        if path.as_os_str().is_empty() {
            continue;
        }
        if path.components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("unsafe path in archive: {}", path.display());
        }
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}
