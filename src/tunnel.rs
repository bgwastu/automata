//! One remotely managed Cloudflare Tunnel for every route, plus the DNS record for Automata's own domain.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{BackupRepo, Config, Keep};
use crate::ctx::{Ctx, DnsRecord, Tunnel};
use crate::docker::{self, docker};

const CONTAINER: &str = "automata-cloudflared";
const OWNER: &str = "managed by automata";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Route {
    pub hostname: String,
    pub service: String,
    /// Automata keeps a CNAME for this hostname. Other hostnames are the user's to point at the tunnel.
    pub dns: bool,
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(Deserialize)]
struct CfError {
    message: String,
}

/// The Cloudflare API with the token from the root .env.
pub struct Api<'a> {
    http: &'a reqwest::Client,
    token: &'a str,
}

impl<'a> Api<'a> {
    pub fn new(ctx: &'a Ctx, config: &'a Config) -> Result<Api<'a>> {
        let token = config.secrets.cloudflare_token.as_deref().context("set CLOUDFLARE_API_TOKEN in the root .env")?;
        Ok(Api { http: &ctx.http, token })
    }

    async fn call<T: DeserializeOwned>(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<T> {
        let mut req = self
            .http
            .request(method.clone(), format!("https://api.cloudflare.com/client/v4{path}"))
            .bearer_auth(self.token)
            .timeout(Duration::from_secs(20));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status();
        let env: Envelope<T> =
            res.json().await.with_context(|| format!("cloudflare {method} {path}: HTTP {status}"))?;
        if !env.success {
            let msg: Vec<_> = env.errors.into_iter().map(|e| e.message).collect();
            bail!("cloudflare {method} {}: {}", path.split('?').next().unwrap_or(path), msg.join("; "));
        }
        env.result.context("cloudflare returned no result")
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(reqwest::Method::GET, path, None).await
    }
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

/// The configured account, or the only one the token can see.
pub async fn account(api: &Api<'_>, config: &Config) -> Result<String> {
    if let Some(a) = &config.cloudflare_account {
        return Ok(a.clone());
    }
    let found: Vec<Id> = api.get("/accounts").await?;
    match &found[..] {
        [one] => Ok(one.id.clone()),
        _ => bail!("the token can see {} accounts; set cloudflare_account in automata.yml", found.len()),
    }
}

/// Find or create the tunnel, and keep a cloudflared container running for it.
pub async fn ensure(ctx: &Ctx, api: &Api<'_>, config: &Config, gateway: &str) -> Result<()> {
    let name = config.tunnel.as_str();
    let have = ctx
        .state(|s| s.tunnel.clone())
        .filter(|t| t.name == name && config.cloudflare_account.as_ref().is_none_or(|a| *a == t.account));
    if have.is_none() {
        let account = account(api, config).await?;
        let base = format!("/accounts/{account}/cfd_tunnel");
        let found: Vec<Id> = api.get(&format!("{base}?name={name}&is_deleted=false")).await?;
        let id = match found.into_iter().next() {
            Some(t) => t.id,
            None => {
                let body = json!({ "name": name, "config_src": "cloudflare" });
                api.call::<Id>(reqwest::Method::POST, &base, Some(body)).await?.id
            }
        };
        let token: String = api.get(&format!("{base}/{id}/token")).await?;
        ctx.state(|s| {
            s.tunnel = Some(Tunnel { id, name: name.into(), account, token });
            s.routes = None;
        });
        ctx.save().await;
    }
    let tunnel = ctx.state(|s| s.tunnel.clone()).expect("tunnel was just set");

    // The label records what the container was started with, so a change recreates it.
    let want = format!("{}@{gateway}", tunnel.id);
    let format = r#"{{.State.Running}} {{index .Config.Labels "automata.tunnel"}}"#;
    let have = docker::output(docker(["inspect", "-f", format, CONTAINER])).await.unwrap_or_default();
    if have.trim() == format!("true {want}") {
        return Ok(());
    }
    let _ = docker::output(docker(["rm", "-f", CONTAINER])).await;
    let mut run = docker([
        "run",
        "-d",
        "--name",
        CONTAINER,
        "--restart",
        "unless-stopped",
        "--network",
        "automata",
        "--add-host",
        &format!("automata-host:{gateway}"),
        "--label",
        &format!("automata.tunnel={want}"),
        "--log-opt",
        "max-size=10m",
        "--log-opt",
        "max-file=3",
        "-e",
        "TUNNEL_TOKEN",
        "cloudflare/cloudflared:latest",
        "tunnel",
        "--no-autoupdate",
        "run",
    ]);
    run.env("TUNNEL_TOKEN", &tunnel.token);
    docker::output(run).await?;
    Ok(())
}

/// Point the tunnel at `routes`, and keep a CNAME to it for the routes that ask for one.
pub async fn sync(ctx: &Ctx, api: &Api<'_>, routes: &[Route]) -> Result<()> {
    let Tunnel { id, account, .. } = ctx.state(|s| s.tunnel.clone()).context("no tunnel")?;
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(&(&id, routes))?));
    if ctx.state(|s| s.routes.clone()).as_deref() == Some(hash.as_str()) {
        return Ok(());
    }
    let mut ingress: Vec<Value> =
        routes.iter().map(|r| json!({ "hostname": r.hostname, "service": r.service })).collect();
    ingress.push(json!({ "service": "http_status:404" }));
    let path = format!("/accounts/{account}/cfd_tunnel/{id}/configurations");
    api.call::<Value>(reqwest::Method::PUT, &path, Some(json!({ "config": { "ingress": ingress } }))).await?;

    let target = format!("{id}.cfargotunnel.com");
    let wanted: BTreeSet<&str> = routes.iter().filter(|r| r.dns).map(|r| r.hostname.as_str()).collect();
    let mut errors = vec![];
    for name in &wanted {
        if let Err(e) = ensure_record(ctx, api, name, &target).await {
            errors.push(format!("{e:#}"));
        }
    }
    let stale: Vec<DnsRecord> =
        ctx.state(|s| s.dns.iter().filter(|d| !wanted.contains(d.name.as_str())).cloned().collect());
    for d in stale {
        let path = format!("/zones/{}/dns_records/{}", d.zone, d.id);
        let _ = api.call::<Value>(reqwest::Method::DELETE, &path, None).await;
        ctx.state(|s| s.dns.retain(|x| *x != d));
    }
    if errors.is_empty() {
        ctx.state(|s| s.routes = Some(hash));
    }
    ctx.save().await;
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Record {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    content: String,
    comment: Option<String>,
}

async fn ensure_record(ctx: &Ctx, api: &Api<'_>, name: &str, target: &str) -> Result<()> {
    let zone = zone_of(api, name).await?;
    let found: Vec<Record> = api.get(&format!("/zones/{zone}/dns_records?name={name}")).await?;
    let existing = found.into_iter().next();
    let ours = existing.as_ref().is_some_and(|r| r.comment.as_deref() == Some(OWNER));
    if let Some(r) = &existing {
        if r.kind == "CNAME" && r.content == target {
            if ours {
                remember(ctx, &zone, &r.id, name);
            }
            return Ok(());
        }
        if !ours {
            bail!("{name} already has a {} record; delete it in Cloudflare first", r.kind);
        }
    }
    let body = json!({ "type": "CNAME", "name": name, "content": target, "proxied": true, "comment": OWNER });
    let saved: Id = match existing {
        Some(r) => api.call(reqwest::Method::PUT, &format!("/zones/{zone}/dns_records/{}", r.id), Some(body)).await?,
        None => api.call(reqwest::Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await?,
    };
    remember(ctx, &zone, &saved.id, name);
    Ok(())
}

fn remember(ctx: &Ctx, zone: &str, id: &str, name: &str) {
    ctx.state(|s| {
        s.dns.retain(|d| d.id != id);
        s.dns.push(DnsRecord { zone: zone.into(), id: id.into(), name: name.into() });
    });
}

static ZONES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

async fn zone_of(api: &Api<'_>, hostname: &str) -> Result<String> {
    let parts: Vec<&str> = hostname.trim_start_matches("*.").split('.').collect();
    for i in 0..parts.len().saturating_sub(1) {
        let candidate = parts[i..].join(".");
        if let Some(z) = ZONES.lock().unwrap().get_or_insert_default().get(&candidate) {
            return Ok(z.clone());
        }
        let found: Vec<Id> = api.get(&format!("/zones?name={candidate}")).await?;
        if let Some(z) = found.into_iter().next() {
            ZONES.lock().unwrap().get_or_insert_default().insert(candidate, z.id.clone());
            return Ok(z.id);
        }
    }
    bail!("{hostname}: no Cloudflare zone for this domain in the account")
}

/// The backup bucket, signed with S3 credentials derived from the Cloudflare token.
#[derive(Clone)]
pub struct R2 {
    pub bucket: rusty_s3::Bucket,
    pub creds: rusty_s3::Credentials,
}

/// R2's S3 API takes the Cloudflare token itself: its id is the access key, its SHA-256 the secret.
/// Creates the bucket the first time and keeps its expiry rules in line with `keep`.
pub async fn r2(ctx: &Ctx, config: &Config, repo: &BackupRepo) -> Result<R2> {
    let api = Api::new(ctx, config)?;
    let cache_key = hex::encode(Sha256::digest(format!("{}\0{}\0{:?}", api.token, repo.bucket, repo.keep)));
    let mut cached = ctx.r2.lock().await;
    if let Some((k, r2)) = cached.as_ref()
        && *k == cache_key
    {
        return Ok(r2.clone());
    }
    let account = match ctx.state(|s| s.tunnel.as_ref().map(|t| t.account.clone())).filter(|a| !a.is_empty()) {
        Some(a) => a,
        None => account(&api, config).await?,
    };
    // Account-owned tokens verify under the account, user tokens under the user.
    let verified = match api.get::<Id>(&format!("/accounts/{account}/tokens/verify")).await {
        Ok(t) => t,
        Err(_) => api.get::<Id>("/user/tokens/verify").await?,
    };
    let path = format!("/accounts/{account}/r2/buckets/{}", repo.bucket);
    if api.get::<Value>(&path).await.is_err() {
        let body = json!({ "name": repo.bucket });
        api.call::<Value>(reqwest::Method::POST, &format!("/accounts/{account}/r2/buckets"), Some(body))
            .await
            .context("creating the R2 bucket; the token needs Account > Workers R2 Storage > Edit")?;
    }
    api.call::<Value>(reqwest::Method::PUT, &format!("{path}/lifecycle"), Some(lifecycle(&repo.keep)))
        .await
        .context("setting the bucket's expiry rules")?;
    let endpoint = format!("https://{account}.r2.cloudflarestorage.com").parse()?;
    let r2 = R2 {
        bucket: rusty_s3::Bucket::new(endpoint, rusty_s3::UrlStyle::Path, repo.bucket.clone(), "auto")?,
        creds: rusty_s3::Credentials::new(verified.id, hex::encode(Sha256::digest(api.token))),
    };
    *cached = Some((cache_key, r2.clone()));
    Ok(r2)
}

/// Each tier is a prefix that R2 empties by age, so nothing in Automata ever deletes a backup.
fn lifecycle(keep: &Keep) -> Value {
    const DAY: u64 = 86400;
    let mut rules = vec![json!({
        "id": "automata: abort stale uploads",
        "enabled": true,
        "conditions": { "prefix": "" },
        "abortMultipartUploadsTransition": { "condition": { "type": "Age", "maxAge": DAY } },
    })];
    for (tier, n, days) in [("daily", keep.daily, 1), ("weekly", keep.weekly, 7), ("monthly", keep.monthly, 31)] {
        if n > 0 {
            rules.push(json!({
                "id": format!("automata: keep {tier} for {n}"),
                "enabled": true,
                "conditions": { "prefix": format!("{tier}/") },
                "deleteObjectsTransition": { "condition": { "type": "Age", "maxAge": u64::from(n) * days * DAY } },
            }));
        }
    }
    json!({ "rules": rules })
}
