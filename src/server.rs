//! The dashboard: one page, a JSON view of everything, live updates over SSE, and a few actions.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast::error::RecvError;

use crate::backup;
use crate::ctx::{Ctx, LogEvent, Outcome};
use crate::deploy::{self, compose};
use crate::docker::{self, Container};

type Ctxt = State<Arc<Ctx>>;

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", include_str!("../ui/index.html")) }))
        .route("/style.css", get(|| async { asset("text/css", include_str!("../ui/style.css")) }))
        .route("/app.js", get(|| async { asset("text/javascript", include_str!("../ui/app.js")) }))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/sync", post(sync))
        .route("/api/apps/{name}/task", get(task))
        .route("/api/apps/{name}/logs", get(logs))
        .route("/api/apps/{name}/snapshots", get(snapshots))
        .route("/api/apps/{name}/{action}", post(action))
        .layer(middleware::from_fn_with_state(ctx.clone(), auth))
        .with_state(ctx)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

/// Basic auth when the root .env sets PASSWORD. Writes also need a header a cross-site form cannot send.
async fn auth(State(ctx): Ctxt, req: Request, next: Next) -> Response {
    // Until the infra repo is read there is no way to know whether a password is required.
    let Some(config) = ctx.config() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "starting, the infra repo is not loaded yet").into_response();
    };
    if let Some(password) = &config.secrets.password {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()?.strip_prefix("Basic "))
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()))
            .unwrap_or_default();
        if !bool::from(given.as_bytes().ct_eq(password.as_bytes())) {
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"automata\"")])
                .into_response();
        }
    }
    if req.method() == axum::http::Method::POST && !req.headers().contains_key("x-automata") {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

#[derive(Serialize)]
struct AppView {
    name: String,
    source: Option<String>,
    domains: Vec<String>,
    rev: Option<String>,
    commit: Option<String>,
    deployed_at: Option<i64>,
    failed: Option<String>,
    error: Option<String>,
    busy: Option<String>,
    containers: Vec<ContainerView>,
    backup: Option<BackupView>,
    images: Option<Outcome>,
}

#[derive(Serialize)]
struct ContainerView {
    service: String,
    state: String,
    health: Option<String>,
    status: String,
}

#[derive(Serialize)]
struct BackupView {
    schedule: String,
    next: Option<i64>,
    last: Option<Outcome>,
}

async fn state(State(ctx): Ctxt) -> Json<Value> {
    let containers = docker::containers().await.unwrap_or_default();
    let config = ctx.config();
    let problems = ctx.problems.lock().unwrap().clone();
    let view = |c: &Container| ContainerView {
        service: c.service.clone(),
        state: c.state.clone(),
        health: c.health.clone(),
        status: c.status.clone(),
    };
    let mut apps: Vec<AppView> = vec![];
    for app in config.iter().flat_map(|c| &c.apps) {
        let s = ctx.app(&app.name, |s| s.clone());
        let source = app.source.as_ref().map(|s| match &s.commit {
            Some(c) => format!("{}@{}", s.github, &c[..7]),
            None => format!("{}#{}", s.github, s.branch),
        });
        apps.push(AppView {
            name: app.name.clone(),
            source,
            domains: app.domains.clone(),
            rev: s.current.as_ref().map(|c| c.rev.clone()),
            commit: s.current.as_ref().and_then(|c| c.commit.clone()),
            deployed_at: s.current.as_ref().map(|c| c.at),
            failed: s.failed,
            error: problems.get(&app.name).cloned().or(s.error),
            busy: ctx.busy(&app.name),
            containers: containers.iter().filter(|c| c.app == app.name).map(view).collect(),
            backup: app.backup.as_ref().map(|b| BackupView {
                schedule: b.schedule.clone(),
                next: crate::sched::next(&b.schedule).map(|t| t.timestamp_millis()),
                last: s.backup,
            }),
            images: s.images,
        });
    }
    let invalid: Vec<Value> =
        config.iter().flat_map(|c| &c.invalid).map(|(name, error)| json!({ "name": name, "error": error })).collect();
    let infra = ctx.infra.lock().unwrap().clone();
    let me = ctx.self_status.lock().unwrap().clone();
    Json(json!({
        "version": me.version,
        "latest": me.latest,
        "self_error": me.error,
        "infra": { "repo": ctx.env.infra, "commit": infra.commit, "error": infra.error, "synced_at": infra.synced_at },
        "dashboard": config.as_ref().and_then(|c| c.dashboard.clone()),
        "tunnel_error": ctx.tunnel_error.lock().unwrap().clone(),
        "busy": ctx.busy("automata").or_else(|| ctx.busy("restic")),
        "apps": apps,
        "invalid": invalid,
    }))
}

/// A "change" event whenever anything the dashboard shows may have changed, at most four a second.
async fn events(State(ctx): Ctxt) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = ctx.changed.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("change").data("1"));
        while rx.changed().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            rx.borrow_and_update();
            yield Ok(Event::default().event("change").data("1"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Pull now, and retry deploys that failed.
async fn sync(State(ctx): Ctxt) -> StatusCode {
    ctx.state(|s| s.apps.values_mut().for_each(|a| a.failed = None));
    ctx.save().await;
    ctx.reconcile();
    StatusCode::ACCEPTED
}

/// Output of the app's last or current task, then new lines as they come.
async fn task(State(ctx): Ctxt, Path(name): Path<String>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (lines, mut rx) = ctx.log_follow(&name);
    let stream = async_stream::stream! {
        for l in lines {
            yield Ok(Event::default().data(l));
        }
        loop {
            match rx.recv().await {
                Ok(LogEvent::Line(l)) => yield Ok(Event::default().data(l)),
                Ok(LogEvent::Clear) => yield Ok(Event::default().event("clear").data("1")),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The app's container logs, followed until the browser disconnects.
async fn logs(State(ctx): Ctxt, Path(name): Path<String>) -> Response {
    let Some(cur) = ctx.state(|s| s.apps.get(&name).and_then(|a| a.current.clone())) else {
        return (StatusCode::NOT_FOUND, "not deployed").into_response();
    };
    let cmd = compose(&name, &cur, &["logs", "-f", "--tail", "300", "--no-color", "--no-log-prefix"]);
    let stream = async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut cmd = cmd;
        let Ok(mut child) = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() else { return };
        docker::forward(child.stdout.take().unwrap(), tx.clone());
        docker::forward(child.stderr.take().unwrap(), tx);
        while let Some(line) = rx.recv().await {
            yield Ok::<_, Infallible>(Event::default().data(line));
        }
        let _ = child.wait().await;
    };
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn snapshots(State(ctx): Ctxt, Path(name): Path<String>) -> Response {
    let Some(repo) = ctx.config().and_then(|c| c.backup.clone()) else {
        return Json(json!([])).into_response();
    };
    match backup::snapshots(&ctx, &repo, &name).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct RestoreQuery {
    snapshot: Option<String>,
}

/// Start a task in the background. Its output shows up on the task stream.
async fn action(
    State(ctx): Ctxt,
    Path((name, action)): Path<(String, String)>,
    Query(q): Query<RestoreQuery>,
) -> Response {
    let config = ctx.config();
    let Some(app) = config.as_ref().and_then(|c| c.apps.iter().find(|a| a.name == name)).cloned() else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };
    if let Some(b) = ctx.busy(&name) {
        return (StatusCode::CONFLICT, b).into_response();
    }
    let repo = config.as_ref().and_then(|c| c.backup.clone());
    let report = |ctx: &Ctx, name: &str, r: anyhow::Result<()>| {
        if let Err(e) = r {
            ctx.log(name, format!("failed: {e:#}"));
        }
    };
    match (action.as_str(), repo) {
        ("restart", _) => {
            tokio::spawn(async move {
                report(&ctx, &name, ctx.queue(&name, "restart", deploy::restart(&ctx, &name)).await)
            });
        }
        ("backup", Some(repo)) if app.backup.is_some() => {
            tokio::spawn(async move { ctx.queue(&name, "backup", backup::backup(&ctx, &repo, &app)).await });
        }
        ("restore", Some(repo)) => {
            let Some(id) = q.snapshot else { return (StatusCode::BAD_REQUEST, "snapshot is required").into_response() };
            tokio::spawn(async move {
                report(&ctx, &name, ctx.queue(&name, "restore", backup::restore(&ctx, &repo, &name, &id)).await)
            });
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    StatusCode::ACCEPTED.into_response()
}
