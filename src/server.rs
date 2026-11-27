//! The dashboard: one page, a JSON view of everything, live updates over SSE, and a few actions.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::ctx::{Ctx, LogEvent, Outcome};
use crate::deploy::{self, compose};
use crate::docker::{self, Container};
use crate::{access, backup, session};

type Ctxt = State<Arc<Ctx>>;

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", include_str!("../ui/index.html")) }))
        .route("/style.css", get(|| async { asset("text/css", include_str!("../ui/style.css")) }))
        .route("/app.js", get(|| async { asset("text/javascript", include_str!("../ui/app.js")) }))
        .route("/api/dashboard/view.js", get(|| async { asset("text/javascript", include_str!("../ui/view.js")) }))
        .route("/api/dashboard/logs.js", get(|| async { asset("text/javascript", include_str!("../ui/logs.js")) }))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/sync", post(sync))
        .route("/api/apps/{name}/task", get(task))
        .route("/api/apps/{name}/logs", get(logs))
        .route("/api/apps/{name}/snapshots", get(snapshots))
        .route("/api/apps/{name}/{action}", post(action))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .layer(middleware::from_fn_with_state(ctx.clone(), auth))
        // Outside auth: app paths are public, like the apps' own domains.
        .layer(middleware::from_fn_with_state(ctx.clone(), crate::proxy::layer))
        .with_state(ctx)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

/// Pages that open without signing in: the login form needs its stylesheet.
/// Who is signed in, from the Cloudflare Access token. `None` without Access.
#[derive(Clone)]
struct User(Option<String>);

/// Two independent locks, either or both of which may be on:
///
/// - Cloudflare Access, when an Access application covers the domain. Every request needs a token
///   Access signed for it, checked here because Access itself only guards `/`.
/// - DASHBOARD_PASSWORD in the root `encrypted_env`. Everything but the login page needs a
///   session cookie.
///
/// With neither, the dashboard is open, and it says so. Writes also need a header a cross-site
/// form cannot send.
async fn auth(State(ctx): Ctxt, mut req: Request, next: Next) -> Response {
    // Until the infra repo is read there is no way to know what is required.
    let Some(config) = ctx.config() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "starting, the infra repo is not loaded yet").into_response();
    };
    let path = req.uri().path();
    if path == "/style.css" {
        return next.run(req).await;
    }
    let api = path.starts_with("/api/") || req.method() != Method::GET;
    let password = config.secrets.password.as_deref();
    let user = match ctx.access.mode() {
        access::Mode::On { team, aud } => {
            let Some(token) = access::token(req.headers()) else {
                return denied(
                    api,
                    "This request did not come through Cloudflare Access. Open the dashboard at its domain to sign in.",
                );
            };
            match access::verify(&ctx, &team, &aud, token).await {
                Ok(user) => Some(user),
                Err(e) => {
                    tracing::warn!("cloudflare access token from {} rejected: {e:#}", session::client(req.headers()));
                    return denied(
                        api,
                        &format!(
                            "Cloudflare Access did not sign you in for this dashboard ({e}). Open the dashboard again to sign in."
                        ),
                    );
                }
            }
        }
        // Without a password, Access is the only lock, so a failed lookup must not open the door.
        access::Mode::Unknown | access::Mode::Failed(_) if password.is_none() => {
            let why = match ctx.access.mode() {
                access::Mode::Failed(e) => {
                    format!("Could not check whether Cloudflare Access protects the dashboard: {e}")
                }
                _ => "Checking whether Cloudflare Access protects the dashboard. Try again in a few seconds.".into(),
            };
            return page(StatusCode::SERVICE_UNAVAILABLE, api, "Not available", &why);
        }
        _ => None,
    };
    if let Some(password) = password
        && path != "/login"
        && !session::signed_in(req.headers(), password)
    {
        if api {
            return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
        }
        return Redirect::to("/login").into_response();
    }
    if req.method() == Method::POST && path != "/login" && !req.headers().contains_key("x-automata") {
        return StatusCode::FORBIDDEN.into_response();
    }
    req.extensions_mut().insert(User(user));
    next.run(req).await
}

fn denied(api: bool, why: &str) -> Response {
    if api {
        return (StatusCode::UNAUTHORIZED, why.to_string()).into_response();
    }
    page(StatusCode::FORBIDDEN, false, "Not signed in", why)
}

/// A short page explaining why the dashboard is not shown, or plain text for API calls.
fn page(status: StatusCode, api: bool, title: &str, why: &str) -> Response {
    if api {
        return (status, why.to_string()).into_response();
    }
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let page =
        include_str!("../ui/notice.html").replace("<!--title-->", &esc(title)).replace("<!--message-->", &esc(why));
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], page)
        .into_response()
}

fn login_html(status: StatusCode, message: &str) -> Response {
    let message = if message.is_empty() { String::new() } else { format!("<p class=\"bad\">{message}</p>") };
    let page = include_str!("../ui/login.html").replace("<!--message-->", &message);
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], page)
        .into_response()
}

async fn login_page(State(ctx): Ctxt, headers: HeaderMap) -> Response {
    match ctx.config().and_then(|c| c.secrets.password.clone()) {
        Some(p) if !session::signed_in(&headers, &p) => login_html(StatusCode::OK, ""),
        _ => Redirect::to("/").into_response(),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login(State(ctx): Ctxt, headers: HeaderMap, Form(form): Form<LoginForm>) -> Response {
    let Some(password) = ctx.config().and_then(|c| c.secrets.password.clone()) else {
        return Redirect::to("/").into_response();
    };
    let client = session::client(&headers);
    if let Some(wait) = session::locked(&client) {
        let minutes = wait.as_secs().div_ceil(60);
        let message =
            format!("Too many wrong passwords. Try again in {minutes} minute{}.", if minutes == 1 { "" } else { "s" });
        return login_html(StatusCode::TOO_MANY_REQUESTS, &message);
    }
    if !session::password_matches(&form.password, &password) {
        session::failed(&client);
        tracing::warn!("wrong dashboard password from {client}");
        // Slows down guessing without making the owner wait long.
        tokio::time::sleep(Duration::from_millis(500)).await;
        return login_html(StatusCode::UNAUTHORIZED, "Wrong password.");
    }
    session::succeeded(&client);
    let cookie = session::set_cookie(&headers, Some(&session::issue(&password)));
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn logout(headers: HeaderMap) -> Response {
    ([(header::SET_COOKIE, session::set_cookie(&headers, None))], StatusCode::NO_CONTENT).into_response()
}

#[derive(Serialize)]
struct AppView {
    name: String,
    source: Option<String>,
    /// Where the app is reachable, without the scheme.
    urls: Vec<String>,
    rev: Option<String>,
    commit: Option<String>,
    /// Infra repo commit the running release came from.
    infra_commit: Option<String>,
    image: Option<String>,
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
    id: String,
    name: String,
    service: String,
    state: String,
    health: Option<String>,
    status: String,
    image: String,
}

#[derive(Serialize)]
struct BackupView {
    schedule: String,
    next: Option<i64>,
    last: Option<Outcome>,
}

async fn state(State(ctx): Ctxt, user: Option<axum::Extension<User>>) -> Json<Value> {
    let (containers, runtime_error) = match docker::containers().await {
        Ok(containers) => (containers, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let config = ctx.config();
    let problems = ctx.problems.lock().unwrap().clone();
    let view = |c: &Container| ContainerView {
        id: c.id.clone(),
        name: c.name.clone(),
        service: c.service.clone(),
        state: c.state.clone(),
        health: c.health.clone(),
        status: c.status.clone(),
        image: c.image.clone(),
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
            urls: config
                .as_ref()
                .and_then(|c| c.domain.as_ref())
                .zip(app.path.as_ref())
                .map(|(d, p)| format!("{d}{p}"))
                .into_iter()
                .chain(app.domains.iter().cloned())
                .collect(),
            rev: s.current.as_ref().map(|c| c.rev.clone()),
            commit: s.current.as_ref().and_then(|c| c.commit.clone()),
            infra_commit: s.current.as_ref().and_then(|c| c.infra.clone()),
            image: app.image.clone(),
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
    let applied = ctx.state(|s| s.infra_applied.clone());
    let me = ctx.self_status.lock().unwrap().clone();
    Json(json!({
        "version": me.version,
        "latest": me.latest,
        "self_error": me.error,
        "infra": {
            "repo": ctx.env.infra, "commit": infra.commit, "error": infra.error, "synced_at": infra.synced_at,
            "syncing": infra.syncing, "requested": infra.requested, "completed": infra.completed,
            "result": infra.result, "applied": applied,
        },
        "runtime_error": runtime_error,
        "domain": config.as_ref().and_then(|c| c.domain.clone()),
        "auth": {
            "password": config.as_ref().is_some_and(|c| c.secrets.password.is_some()),
            "access": match ctx.access.mode() {
                access::Mode::On { .. } => "on",
                access::Mode::Off => "off",
                access::Mode::Unknown => "checking",
                access::Mode::Failed(_) => "failed",
            },
            "access_error": match ctx.access.mode() { access::Mode::Failed(e) => Some(e), _ => None },
            "user": user.and_then(|u| u.0.0.clone()),
        },
        "tunnel_error": ctx.tunnel_error.lock().unwrap().clone(),
        "busy": ctx.busy("automata"),
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
async fn sync(State(ctx): Ctxt) -> (StatusCode, Json<Value>) {
    let request = {
        let mut infra = ctx.infra.lock().unwrap();
        infra.requested += 1;
        infra.requested
    };
    ctx.reconcile();
    ctx.changed();
    (StatusCode::ACCEPTED, Json(json!({ "request": request })))
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
async fn logs(State(ctx): Ctxt, Path(name): Path<String>, Query(query): Query<LogQuery>) -> Response {
    let report_end = query.details || query.container.is_some();
    let cmd = if let Some(container) = query.container {
        let Some(id) = docker::app_container_id(&name, &container).await else {
            return (StatusCode::NOT_FOUND, "container not available for this app").into_response();
        };
        docker::docker(["logs", "--follow", "--tail", "300", "--timestamps", &id])
    } else {
        let Some(cur) = ctx.state(|s| s.apps.get(&name).and_then(|a| a.current.clone())) else {
            return (StatusCode::NOT_FOUND, "not deployed").into_response();
        };
        let format = if query.details { "--timestamps" } else { "--no-log-prefix" };
        compose(&name, &cur, &["logs", "-f", "--tail", "300", "--no-color", format])
    };
    let stream = async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut cmd = cmd;
        let Ok(mut child) = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() else {
            if report_end {
                yield Ok::<_, Infallible>(Event::default().event("log-error").data("Could not start Docker logs."));
            }
            return;
        };
        docker::forward(child.stdout.take().unwrap(), tx.clone());
        docker::forward(child.stderr.take().unwrap(), tx);
        while let Some(line) = rx.recv().await {
            yield Ok::<_, Infallible>(Event::default().data(line));
        }
        let success = child.wait().await.is_ok_and(|status| status.success());
        if report_end {
            yield Ok::<_, Infallible>(if success {
                Event::default().event("end").data("1")
            } else {
                Event::default().event("log-error").data("Docker could not read or follow these logs.")
            });
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Deserialize)]
struct LogQuery {
    container: Option<String>,
    #[serde(default)]
    details: bool,
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
