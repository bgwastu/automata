//! Volume backups: each run uploads a gzipped tar of the app's volumes to R2 as one object,
//! `<tier>/<app>/<time>.tar.gz`. R2 expires each tier by age, so a backup is a plain file you can download.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Utc};
use rusty_s3::S3Action;
use rusty_s3::actions::ListObjectsV2;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::config::{App, BackupRepo, Keep};
use crate::ctx::{Ctx, Outcome, now_ms};
use crate::deploy::compose;
use crate::docker::{self, docker};
use crate::tunnel::R2;

const IMAGE: &str = "alpine:3";
/// R2 wants every part but the last to be the same size.
const PART: usize = 16 << 20;
const SIGNED: Duration = Duration::from_secs(3600);
const TIERS: [&str; 3] = ["daily", "weekly", "monthly"];
const TIME: &str = "%Y%m%dT%H%M%SZ";

/// A throwaway container with the volumes mounted, running `script` at the lowest CPU priority.
fn helper(mounts: &[String], script: &str) -> Command {
    let mut a: Vec<String> = ["run", "--rm", "-i", "--cpu-shares", "128"].map(String::from).to_vec();
    for m in mounts {
        a.extend(["-v".into(), m.clone()]);
    }
    a.extend([IMAGE, "nice", "-n", "19", "sh", "-c", script].map(String::from));
    docker(a)
}

/// The first run on the 1st of a month is monthly, the first on a Sunday weekly, the rest daily.
fn tier(keep: &Keep, now: chrono::DateTime<Utc>, earlier_today: bool) -> &'static str {
    if !earlier_today && keep.monthly > 0 && now.day() == 1 {
        "monthly"
    } else if !earlier_today && keep.weekly > 0 && now.weekday() == chrono::Weekday::Sun {
        "weekly"
    } else {
        "daily"
    }
}

pub async fn backup(ctx: &Ctx, repo: &BackupRepo, app: &App) {
    let name = &app.name;
    ctx.log_clear(name);
    let result = async {
        let vols = docker::volumes(name).await?;
        if vols.is_empty() {
            bail!("app has no volumes to back up");
        }
        let config = ctx.config().context("the infra repo is not loaded")?;
        let r2 = crate::tunnel::r2(ctx, &config, repo).await?;
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

        let now = Utc::now();
        let today = ctx.app(name, |s| {
            s.backup.as_ref().is_some_and(|b| {
                b.ok && chrono::DateTime::from_timestamp_millis(b.at)
                    .is_some_and(|t| t.date_naive() == now.date_naive())
            })
        });
        let key = format!("{}/{name}/{}.tar.gz", tier(&repo.keep, now, today), now.format(TIME));
        ctx.log(name, format!("uploading {} to {key}", vols.join(", ")));

        let mounts: Vec<String> = vols.iter().map(|v| format!("{v}:/volumes/{v}:ro")).collect();
        let mut child = helper(&mounts, "cd /volumes && tar -c . | gzip -6")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        docker::forward(child.stderr.take().unwrap(), tx);
        let uploaded = upload(&ctx.http, &r2, &key, child.stdout.take().unwrap()).await;
        if uploaded.is_err() {
            let _ = child.kill().await;
        }
        let status = child.wait().await?;
        while let Ok(l) = rx.try_recv() {
            ctx.log(name, l);
        }
        let size = uploaded?;
        if !status.success() {
            // A failed tar still uploaded something. Do not leave a broken backup behind.
            let url = r2.bucket.delete_object(Some(&r2.creds), &key).sign(SIGNED);
            let _ = ctx.http.delete(url).send().await;
            bail!("tar exited with {status}");
        }
        ctx.log(name, format!("uploaded {}", human(size)));
        Ok(())
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

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
    }
}

async fn read_part(body: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(PART);
    body.take(PART as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Stream `body` to `key`: one PUT when it is small, a multipart upload otherwise. Returns the size.
async fn upload(http: &reqwest::Client, r2: &R2, key: &str, mut body: impl AsyncRead + Unpin) -> Result<u64> {
    let first = read_part(&mut body).await?;
    if first.len() < PART {
        let url = r2.bucket.put_object(Some(&r2.creds), key).sign(SIGNED);
        let size = first.len() as u64;
        http.put(url)
            .body(first)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.without_url())
            .context("uploading")?;
        return Ok(size);
    }

    let url = r2.bucket.create_multipart_upload(Some(&r2.creds), key).sign(SIGNED);
    let res = http
        .post(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url())
        .context("starting the upload")?
        .text()
        .await
        .map_err(|e| e.without_url())?;
    let id = rusty_s3::actions::CreateMultipartUpload::parse_response(&res)?.upload_id().to_string();
    let parts = async {
        let mut etags = vec![];
        let mut size = 0u64;
        let mut chunk = first;
        while !chunk.is_empty() {
            size += chunk.len() as u64;
            let n = u16::try_from(etags.len() + 1).context("backup is over 10000 parts")?;
            let url = r2.bucket.upload_part(Some(&r2.creds), key, n, &id).sign(SIGNED);
            let res = http
                .put(url)
                .body(chunk)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.without_url())
                .context("uploading")?;
            let etag = res.headers().get("etag").and_then(|e| e.to_str().ok()).context("part has no etag")?;
            etags.push(etag.to_string());
            chunk = read_part(&mut body).await?;
        }
        let done = r2.bucket.complete_multipart_upload(Some(&r2.creds), key, &id, etags.iter().map(String::as_str));
        let url = done.sign(SIGNED);
        http.post(url)
            .body(done.body())
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.without_url())
            .context("finishing the upload")?;
        anyhow::Ok(size)
    };
    let result = parts.await;
    if result.is_err() {
        let url = r2.bucket.abort_multipart_upload(Some(&r2.creds), key, &id).sign(SIGNED);
        let _ = http.delete(url).send().await;
    }
    result
}

#[derive(Serialize)]
pub struct Snapshot {
    /// `<tier>/<time>`
    id: String,
    time: String,
    size: u64,
}

pub async fn snapshots(ctx: &Ctx, repo: &BackupRepo, name: &str) -> Result<Vec<Snapshot>> {
    let config = ctx.config().context("the infra repo is not loaded")?;
    let r2 = crate::tunnel::r2(ctx, &config, repo).await?;
    let mut out = vec![];
    for tier in TIERS {
        let prefix = format!("{tier}/{name}/");
        let mut token: Option<String> = None;
        loop {
            let mut list = r2.bucket.list_objects_v2(Some(&r2.creds));
            list.with_prefix(prefix.as_str());
            if let Some(t) = &token {
                list.with_continuation_token(t.clone());
            }
            let res = ctx
                .http
                .get(list.sign(SIGNED))
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.without_url())?
                .text()
                .await
                .map_err(|e| e.without_url())?;
            let page = ListObjectsV2::parse_response(&res)?;
            for o in page.contents {
                let Some(stamp) = o.key.strip_prefix(&prefix).and_then(|k| k.strip_suffix(".tar.gz")) else { continue };
                let Ok(t) = chrono::NaiveDateTime::parse_from_str(stamp, TIME) else { continue };
                out.push(Snapshot { id: format!("{tier}/{stamp}"), time: t.and_utc().to_rfc3339(), size: o.size });
            }
            match page.next_continuation_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(out)
}

/// Stop the app, replace its volumes with the snapshot, start it again.
pub async fn restore(ctx: &Ctx, repo: &BackupRepo, name: &str, snapshot: &str) -> Result<()> {
    let Some((tier, stamp)) = snapshot
        .split_once('/')
        .filter(|(t, s)| TIERS.contains(t) && chrono::NaiveDateTime::parse_from_str(s, TIME).is_ok())
    else {
        bail!("bad snapshot id");
    };
    let cur = ctx.app(name, |s| s.current.clone()).context("app is not deployed")?;
    ctx.log_clear(name);
    let config = ctx.config().context("the infra repo is not loaded")?;
    let r2 = crate::tunnel::r2(ctx, &config, repo).await?;
    let key = format!("{tier}/{name}/{stamp}.tar.gz");
    // Start the download before stopping, so a missing snapshot leaves the app running.
    let url = r2.bucket.get_object(Some(&r2.creds), &key).sign(SIGNED);
    let mut res = ctx
        .http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url())
        .with_context(|| format!("downloading {key}"))?;

    let vols = docker::volumes(name).await?;
    ctx.log(name, format!("stopping {name}"));
    docker::must(compose(name, &cur, &["stop"]), |l| ctx.log(name, l)).await?;
    let restored = async {
        ctx.log(name, format!("restoring {key}"));
        let mounts: Vec<String> = vols.iter().map(|v| format!("{v}:/volumes/{v}")).collect();
        let script =
            r#"for d in /volumes/*/; do find "$d" -mindepth 1 -delete; done; tar -xz --numeric-owner -C /volumes"#;
        let mut child =
            helper(&mounts, script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        docker::forward(child.stdout.take().unwrap(), tx.clone());
        docker::forward(child.stderr.take().unwrap(), tx);
        let mut stdin = child.stdin.take().unwrap();
        let copied = async {
            while let Some(chunk) = res.chunk().await.map_err(|e| e.without_url())? {
                stdin.write_all(&chunk).await?;
            }
            drop(stdin);
            anyhow::Ok(())
        }
        .await;
        let status = child.wait().await?;
        while let Ok(l) = rx.try_recv() {
            ctx.log(name, l);
        }
        copied?;
        if !status.success() {
            bail!("tar exited with {status}");
        }
        anyhow::Ok(())
    }
    .await;
    docker::must(compose(name, &cur, &["start"]), |l| ctx.log(name, l)).await?;
    restored?;
    ctx.log(name, "restored");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn tiers() {
        let keep = Keep::default();
        let first = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap(); // a Sunday
        assert_eq!(tier(&keep, first, false), "monthly");
        assert_eq!(tier(&keep, first, true), "daily");
        assert_eq!(tier(&Keep { monthly: 0, ..keep.clone() }, first, false), "weekly");
        assert_eq!(tier(&keep, Utc.with_ymd_and_hms(2026, 11, 8, 3, 0, 0).unwrap(), false), "weekly");
        assert_eq!(tier(&keep, Utc.with_ymd_and_hms(2026, 11, 9, 3, 0, 0).unwrap(), false), "daily");
    }
}
