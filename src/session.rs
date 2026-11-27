//! Dashboard sign-in: a login form that sets a signed session cookie.
//!
//! The cookie holds an expiry time and an HMAC of it. The HMAC key is random and kept in the data
//! folder, and the password's hash is part of what is signed, so a new password signs everyone
//! out, and so does deleting `session.key`. Nothing about sessions is kept in memory, so they
//! survive restarts and self-updates.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::http::{HeaderMap, header};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// `__Host-` makes browsers refuse it unless it is Secure, for `/`, and set by this exact host.
pub const SECURE_NAME: &str = "__Host-automata_session";
/// Over plain HTTP on localhost or an IP, where a Secure cookie would not be sent back.
pub const PLAIN_NAME: &str = "automata_session";
pub const LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);

/// Failed logins from one address before it has to wait.
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

static KEY: OnceLock<[u8; 32]> = OnceLock::new();
static FAILURES: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

/// Load the signing key from the data folder, creating it on first run.
pub fn init(data: &Path) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let path = data.join("session.key");
    let key = match std::fs::read(&path) {
        Ok(k) if k.len() == 32 => k,
        _ => {
            let mut k = vec![0u8; 32];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut k)?;
            std::fs::create_dir_all(data)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            f.write_all(&k)?;
            k
        }
    };
    let _ = KEY.set(key.try_into().unwrap());
    Ok(())
}

fn mac(password: &str, expires: u64) -> Hmac<Sha256> {
    let key = KEY.get().expect("session::init was not called");
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(format!("v1\0{expires}\0").as_bytes());
    m.update(&Sha256::digest(password.as_bytes()));
    m
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A new session cookie value.
pub fn issue(password: &str) -> String {
    let expires = now() + LIFETIME.as_secs();
    format!("v1.{expires}.{}", hex::encode(mac(password, expires).finalize().into_bytes()))
}

fn valid(password: &str, value: &str) -> bool {
    let mut parts = value.split('.');
    let (Some("v1"), Some(expires), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(expires), Ok(sig)) = (expires.parse::<u64>(), hex::decode(sig)) else { return false };
    expires > now() && mac(password, expires).verify_slice(&sig).is_ok()
}

/// Whether the request carries a valid session cookie.
pub fn signed_in(headers: &HeaderMap, password: &str) -> bool {
    cookies(headers).any(|(name, value)| (name == SECURE_NAME || name == PLAIN_NAME) && valid(password, value))
}

pub fn cookies(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|c| c.trim().split_once('='))
}

/// Whether the browser reached us over HTTPS, judged by the host: the dashboard domain always comes
/// through Cloudflare, while localhost and bare addresses mean a direct connection or SSH tunnel.
pub fn secure(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    !(host.is_empty() || host == "localhost" || host.parse::<std::net::IpAddr>().is_ok())
}

/// The `Set-Cookie` value that signs the browser in, or out when `value` is `None`.
pub fn set_cookie(headers: &HeaderMap, value: Option<&str>) -> String {
    let (name, flags) = if secure(headers) { (SECURE_NAME, "; Secure") } else { (PLAIN_NAME, "") };
    let (value, age) = match value {
        Some(v) => (v, LIFETIME.as_secs()),
        None => ("", 0),
    };
    format!("{name}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Strict{flags}")
}

pub fn password_matches(given: &str, password: &str) -> bool {
    // Hashing first keeps the comparison constant-time regardless of length.
    Sha256::digest(given.as_bytes()).ct_eq(&Sha256::digest(password.as_bytes())).into()
}

/// The visitor's address. Cloudflare sets this header and replaces any the visitor sends.
/// Without it the request came from the host itself.
pub fn client(headers: &HeaderMap) -> String {
    headers.get("cf-connecting-ip").and_then(|h| h.to_str().ok()).unwrap_or("local").to_string()
}

/// How long this address must wait before trying again, if it is locked out.
pub fn locked(client: &str) -> Option<Duration> {
    let mut map = FAILURES.lock().unwrap();
    let map = map.get_or_insert_default();
    map.retain(|_, (_, first)| first.elapsed() < LOCKOUT);
    let (count, first) = map.get(client)?;
    (*count >= MAX_FAILURES).then(|| LOCKOUT.saturating_sub(first.elapsed()))
}

pub fn failed(client: &str) {
    let mut map = FAILURES.lock().unwrap();
    let entry = map.get_or_insert_default().entry(client.to_string()).or_insert((0, Instant::now()));
    entry.0 += 1;
}

pub fn succeeded(client: &str) {
    FAILURES.lock().unwrap().get_or_insert_default().remove(client);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn with(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn cookie() {
        let _ = KEY.set([7; 32]);
        let v = issue("pw");
        assert!(valid("pw", &v));
        assert!(!valid("other", &v), "a new password signs everyone out");
        let (rest, sig) = v.rsplit_once('.').unwrap();
        let later = rest.replace("v1.", "v1.9");
        assert!(!valid("pw", &format!("{later}.{sig}")), "the expiry is signed");
        assert!(!valid("pw", "v1.99999999999.00"));
        assert!(!valid("pw", "v1.1.") && !valid("pw", ""));
        let expired = 1;
        let old = format!("v1.{expired}.{}", hex::encode(mac("pw", expired).finalize().into_bytes()));
        assert!(!valid("pw", &old));

        assert!(signed_in(&with("cookie", &format!("a=b; {SECURE_NAME}={v}")), "pw"));
        assert!(!signed_in(&with("cookie", &format!("other={v}")), "pw"));
    }

    #[test]
    fn secure_hosts() {
        assert!(secure(&with("host", "automata.example.com")));
        for h in ["localhost:8080", "127.0.0.1:8090", "[::1]:8090", "172.30.12.1"] {
            assert!(!secure(&with("host", h)), "{h}");
        }
        assert!(set_cookie(&with("host", "a.example.com"), Some("x")).starts_with("__Host-automata_session=x;"));
        assert!(set_cookie(&with("host", "localhost"), None).contains("Max-Age=0"));
    }

    #[test]
    fn lockout() {
        for _ in 0..MAX_FAILURES - 1 {
            failed("1.2.3.4");
        }
        assert!(locked("1.2.3.4").is_none());
        failed("1.2.3.4");
        assert!(locked("1.2.3.4").is_some());
        assert!(locked("5.6.7.8").is_none());
        succeeded("1.2.3.4");
        assert!(locked("1.2.3.4").is_none());
    }
}
