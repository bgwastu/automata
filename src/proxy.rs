//! Apps with a `path` are served under Automata's domain. Automata forwards `/<path>/rest` to the app
//! as `/rest`, so the app needs no idea of where it is mounted. Websockets and streams pass through.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;

use crate::ctx::Ctx;
use crate::docker::{self, docker};
use crate::session;

/// Container addresses, looked up again after this long or after a failed connection.
static ADDRS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
const FRESH: Duration = Duration::from_secs(30);

pub async fn layer(State(ctx): State<Arc<Ctx>>, req: Request, next: Next) -> Response {
    let Some(config) = ctx.config() else { return next.run(req).await };
    let path = req.uri().path();
    let found = config.apps.iter().find_map(|a| {
        let prefix = a.path.as_deref()?;
        let rest = path.strip_prefix(prefix)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| (a.name.clone(), a.port, prefix.to_string()))
    });
    let Some((name, port, prefix)) = found else { return next.run(req).await };
    match forward(&name, port, &prefix, req).await {
        Ok(res) => res,
        Err(e) => {
            tracing::warn!("proxy {name}: {e:#}");
            ADDRS.lock().unwrap().get_or_insert_default().remove(&name);
            (StatusCode::BAD_GATEWAY, format!("{name} is not reachable")).into_response()
        }
    }
}

async fn forward(name: &str, port: u16, prefix: &str, mut req: Request) -> Result<Response> {
    let ip = address(name).await?;
    let stream = tokio::net::TcpStream::connect((ip.as_str(), port)).await.context("connecting")?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let rest = &req.uri().path()[prefix.len()..];
    let rest = if rest.is_empty() { "/" } else { rest };
    let target = match req.uri().query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };
    *req.uri_mut() = target.parse::<Uri>()?;
    strip_session(req.headers_mut());
    let headers = req.headers_mut();
    headers.insert("x-forwarded-prefix", HeaderValue::from_str(prefix)?);
    if let Some(host) = headers.get("host").cloned() {
        headers.entry("x-forwarded-host").or_insert(host);
    }
    headers.entry("x-forwarded-proto").or_insert(HeaderValue::from_static("https"));

    let client_upgrade = hyper::upgrade::on(&mut req);
    let mut res = sender.send_request(req).await?;
    if res.status() == StatusCode::SWITCHING_PROTOCOLS {
        let app_upgrade = hyper::upgrade::on(&mut res);
        tokio::spawn(async move {
            if let (Ok(client), Ok(app)) = (client_upgrade.await, app_upgrade.await) {
                let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(app)).await;
            }
        });
    }
    Ok(res.map(Body::new))
}

/// Apps under a path share the dashboard's domain, so the browser sends them the dashboard's
/// session cookie and Cloudflare Access token too. Keep both from them: either one opens the
/// dashboard.
fn strip_session(headers: &mut HeaderMap) {
    headers.remove(crate::access::HEADER);
    let kept: Vec<String> = session::cookies(headers)
        .filter(|(name, _)| ![session::SECURE_NAME, session::PLAIN_NAME, crate::access::COOKIE].contains(name))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    headers.remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
    {
        headers.insert(header::COOKIE, v);
    }
}

/// The app's main container on the shared network. Only that service joins it.
async fn address(name: &str) -> Result<String> {
    if let Some((ip, at)) = ADDRS.lock().unwrap().get_or_insert_default().get(name)
        && at.elapsed() < FRESH
    {
        return Ok(ip.clone());
    }
    let filter = format!("label=automata.app={name}");
    let ids = docker::output(docker(["ps", "-q", "--filter", &filter, "--filter", "network=automata"])).await?;
    let Some(id) = ids.split_whitespace().next() else { bail!("not running") };
    let format = r#"{{(index .NetworkSettings.Networks "automata").IPAddress}}"#;
    let ip = docker::output(docker(["inspect", "-f", format, id])).await?.trim().to_string();
    if ip.is_empty() {
        bail!("no address on the automata network");
    }
    ADDRS.lock().unwrap().get_or_insert_default().insert(name.into(), (ip.clone(), Instant::now()));
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn strips_the_session_cookie() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1; __Host-automata_session=v1.2.3"));
        h.append(header::COOKIE, HeaderValue::from_static("automata_session=x; b=2; CF_Authorization=jwt"));
        h.insert("cf-access-jwt-assertion", HeaderValue::from_static("jwt"));
        super::strip_session(&mut h);
        assert_eq!(h.get_all(header::COOKIE).iter().collect::<Vec<_>>(), ["a=1; b=2"]);
        assert!(h.get("cf-access-jwt-assertion").is_none());
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("automata_session=x"));
        super::strip_session(&mut h);
        assert!(h.get(header::COOKIE).is_none());
    }
}
