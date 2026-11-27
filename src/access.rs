//! Cloudflare Access in front of the dashboard.
//!
//! When an Access application covers Automata's domain, every dashboard request must carry a token
//! Access signed for that application. Access only guards `/` (the rest of the domain is bypassed
//! so path apps stay public), so the token is checked here on every request, not just at the edge.
//! The browser sends it as the `CF_Authorization` cookie, and Access adds the
//! `Cf-Access-Jwt-Assertion` header on requests it handled itself.
//!
//! Nothing is configured by hand: the team domain and the application's audience tag come from
//! the Cloudflare API with the same token Automata uses for the tunnel.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::http::HeaderMap;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::ctx::Ctx;
use crate::session;
use crate::tunnel::{self, Api};

pub const COOKIE: &str = "CF_Authorization";
pub const HEADER: &str = "cf-access-jwt-assertion";

/// How often to look again for an Access application.
const RECHECK: Duration = Duration::from_secs(10 * 60);
/// How long to keep Access's signing keys before fetching them again.
const KEYS_FOR: Duration = Duration::from_secs(3600);
/// The fastest a token signed with an unknown key may trigger a refetch.
const REFETCH_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Mode {
    /// Not looked yet.
    #[default]
    Unknown,
    /// No domain, no Cloudflare token, or no Access application for the domain.
    Off,
    On {
        /// Like `team.cloudflareaccess.com`.
        team: String,
        /// Audience tags of the applications covering the domain.
        aud: Vec<String>,
    },
    /// The lookup failed, so it is not known whether Access is on.
    Failed(String),
}

#[derive(Default)]
pub struct Access {
    mode: Mutex<Mode>,
    /// What the mode was worked out from, and when.
    checked: Mutex<Option<(String, Instant)>>,
    keys: tokio::sync::Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    team: String,
    by_kid: HashMap<String, (Vec<u8>, Vec<u8>)>,
    fetched: Option<Instant>,
}

impl Access {
    pub fn mode(&self) -> Mode {
        self.mode.lock().unwrap().clone()
    }
}

/// Look for the Access application, at most every ten minutes unless the domain or token changed.
pub async fn refresh(ctx: &Ctx, config: &Config) {
    let (Some(domain), Some(token)) = (&config.domain, &config.secrets.cloudflare_token) else {
        *ctx.access.mode.lock().unwrap() = Mode::Off;
        return;
    };
    let key = format!("{domain}\0{}", hex::encode(<sha2::Sha256 as sha2::Digest>::digest(token.as_bytes())));
    let fresh = ctx.access.checked.lock().unwrap().as_ref().is_some_and(|(k, at)| *k == key && at.elapsed() < RECHECK);
    let failed = matches!(ctx.access.mode(), Mode::Failed(_) | Mode::Unknown);
    if fresh && !failed {
        return;
    }
    let mode = match discover(ctx, config, domain).await {
        Ok(Some((team, aud))) => Mode::On { team, aud },
        Ok(None) => Mode::Off,
        Err(e) => Mode::Failed(format!("{e:#}")),
    };
    if let Mode::Failed(e) = &mode {
        tracing::warn!("cloudflare access: {e}");
    }
    let changed = *ctx.access.mode.lock().unwrap() != mode;
    *ctx.access.mode.lock().unwrap() = mode;
    *ctx.access.checked.lock().unwrap() = Some((key, Instant::now()));
    if changed {
        ctx.changed();
    }
}

#[derive(Deserialize)]
struct Org {
    auth_domain: String,
}

#[derive(Deserialize)]
struct App {
    aud: Option<String>,
    domain: Option<String>,
    #[serde(default)]
    self_hosted_domains: Vec<String>,
}

async fn discover(ctx: &Ctx, config: &Config, domain: &str) -> Result<Option<(String, Vec<String>)>> {
    let api = Api::new(ctx, config)?;
    let account = match ctx.state(|s| s.tunnel.clone()).map(|t| t.account).filter(|a| !a.is_empty()) {
        Some(a) => a,
        None => tunnel::account(&api, config).await?,
    };
    let apps: Vec<App> = api
        .get(&format!("/accounts/{account}/access/apps?per_page=1000"))
        .await
        .context("listing Access applications (the token needs Access: Apps and Policies Read)")?;
    let aud: Vec<String> = apps
        .into_iter()
        .filter(|a| a.domain.iter().chain(&a.self_hosted_domains).any(|d| covers(d, domain)))
        .filter_map(|a| a.aud)
        .collect();
    if aud.is_empty() {
        return Ok(None);
    }
    let org: Org = api.get(&format!("/accounts/{account}/access/organizations")).await.context(
        "reading the Access team (the token needs Access: Organizations, Identity Providers, and Groups Read)",
    )?;
    Ok(Some((org.auth_domain, aud)))
}

/// Whether an Access application domain covers the dashboard at `/`. Path-scoped applications,
/// like the bypass ones for path apps, do not.
fn covers(app_domain: &str, domain: &str) -> bool {
    let d = app_domain.trim_end_matches('/');
    d.eq_ignore_ascii_case(domain) || d.strip_suffix("/*").is_some_and(|d| d.eq_ignore_ascii_case(domain))
}

/// The Access token on a request, if any.
pub fn token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(HEADER)
        .and_then(|h| h.to_str().ok())
        .or_else(|| session::cookies(headers).find(|(n, _)| *n == COOKIE).map(|(_, v)| v))
}

/// Check a token and return who it was issued to.
pub async fn verify(ctx: &Ctx, team: &str, aud: &[String], token: &str) -> Result<String> {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        bail!("not a JWT");
    };
    let header: Value = serde_json::from_slice(&b64.decode(h)?)?;
    if header["alg"] != "RS256" {
        bail!("unexpected algorithm {}", header["alg"]);
    }
    let kid = header["kid"].as_str().context("no key id")?;
    let (n, e) = key(ctx, team, kid).await?;
    let sig = b64.decode(s)?;
    ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
        .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| anyhow::anyhow!("bad signature"))?;
    let claims: Value = serde_json::from_slice(&b64.decode(p)?)?;
    check_claims(&claims, team, aud, now())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn check_claims(claims: &Value, team: &str, aud: &[String], now: u64) -> Result<String> {
    if claims["iss"].as_str() != Some(&format!("https://{team}")) {
        bail!("issued by {}", claims["iss"]);
    }
    let auds: Vec<&str> = match &claims["aud"] {
        Value::String(s) => vec![s],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !auds.iter().any(|a| aud.iter().any(|w| w == a)) {
        bail!("issued for another application");
    }
    // A minute of leeway for clock drift.
    if claims["exp"].as_u64().is_none_or(|exp| exp + 60 < now) {
        bail!("expired");
    }
    if claims["nbf"].as_u64().is_some_and(|nbf| nbf > now + 60) {
        bail!("not valid yet");
    }
    claims["email"]
        .as_str()
        .filter(|e| !e.is_empty())
        .or_else(|| claims["common_name"].as_str())
        .map(str::to_string)
        .context("no email or service token name")
}

async fn key(ctx: &Ctx, team: &str, kid: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut keys = ctx.access.keys.lock().await;
    let stale = keys.team != team || keys.fetched.is_none_or(|t| t.elapsed() > KEYS_FOR);
    let unknown = !keys.by_kid.contains_key(kid) && keys.fetched.is_none_or(|t| t.elapsed() > REFETCH_AFTER);
    if stale || unknown {
        *keys = fetch_keys(ctx, team).await?;
    }
    keys.by_kid.get(kid).cloned().context("signed with an unknown key")
}

async fn fetch_keys(ctx: &Ctx, team: &str) -> Result<Keys> {
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<Jwk>,
    }
    #[derive(Deserialize)]
    struct Jwk {
        kid: String,
        kty: String,
        n: String,
        e: String,
    }
    let jwks: Jwks = ctx
        .http
        .get(format!("https://{team}/cdn-cgi/access/certs"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("Access signing keys")?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let by_kid = jwks
        .keys
        .into_iter()
        .filter(|k| k.kty == "RSA")
        .filter_map(|k| Some((k.kid, (b64.decode(k.n).ok()?, b64.decode(k.e).ok()?))))
        .collect();
    Ok(Keys { team: team.into(), by_kid, fetched: Some(Instant::now()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn domains() {
        assert!(covers("automata.example.com", "automata.example.com"));
        assert!(covers("Automata.example.com/", "automata.example.com"));
        assert!(covers("automata.example.com/*", "automata.example.com"));
        assert!(!covers("automata.example.com/a*", "automata.example.com"));
        assert!(!covers("other.example.com", "automata.example.com"));
    }

    #[test]
    fn claims() {
        let aud = vec!["abc".to_string()];
        let good = json!({ "iss": "https://t.cloudflareaccess.com", "aud": ["abc"], "exp": 2000, "email": "a@b.c" });
        assert_eq!(check_claims(&good, "t.cloudflareaccess.com", &aud, 1000).unwrap(), "a@b.c");
        assert!(check_claims(&good, "t.cloudflareaccess.com", &aud, 2100).is_err(), "expired");
        assert!(check_claims(&good, "evil.cloudflareaccess.com", &aud, 1000).is_err(), "issuer");
        assert!(check_claims(&good, "t.cloudflareaccess.com", &["x".into()], 1000).is_err(), "audience");
        let service = json!({ "iss": "https://t.cloudflareaccess.com", "aud": "abc", "exp": 2000, "email": "", "common_name": "ci" });
        assert_eq!(check_claims(&service, "t.cloudflareaccess.com", &aud, 1000).unwrap(), "ci");
        let early =
            json!({ "iss": "https://t.cloudflareaccess.com", "aud": "abc", "exp": 2000, "nbf": 1500, "email": "a" });
        assert!(check_claims(&early, "t.cloudflareaccess.com", &aud, 1000).is_err(), "not before");
    }
}
