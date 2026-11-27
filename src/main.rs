mod access;
mod backup;
mod config;
mod ctx;
mod deploy;
mod docker;
mod git;
mod images;
mod proxy;
mod reconcile;
mod sched;
mod secret;
mod selfupdate;
mod server;
mod session;
mod tunnel;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => serve().await,
        Some("--version" | "-V") => {
            println!("automata {}", selfupdate::VERSION);
            Ok(())
        }
        Some("update") => update().await,
        Some("encrypt") => {
            let mut value = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)?;
            println!("{}", secret::encrypt(value.strip_suffix('\n').unwrap_or(&value))?);
            Ok(())
        }
        Some(other) => bail!("unknown command {other}\nusage: automata [update | encrypt < value | --version]"),
    }
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .init();
    let ctx = ctx::Ctx::new(config::Env::load()?)?;
    session::init(&ctx.env.data)?;

    // Docker may still be starting at boot.
    let gateway = loop {
        match docker::ensure_network(reconcile::NETWORK).await {
            Ok(g) => break g,
            Err(e) => {
                tracing::warn!("waiting for docker: {e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    };

    // Loopback for the host itself, the network gateway for cloudflared.
    let app = server::router(ctx.clone());
    for ip in ["127.0.0.1", gateway.as_str()] {
        let addr = format!("{ip}:{}", ctx.env.port);
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) if ip != "127.0.0.1" => {
                tracing::warn!("cannot listen on {addr}, the dashboard domain will not work: {e}");
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("listening on {addr}")),
        };
        tracing::info!("dashboard on http://{addr}");
        let app = app.clone();
        tokio::spawn(async move { axum::serve(listener, app).await });
    }

    tokio::spawn(sched::run(ctx.clone()));
    tokio::spawn(reconcile::watch_docker(ctx.clone()));
    let check = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        selfupdate::run(&check).await;
    });
    tracing::info!("automata {} watching {}", selfupdate::VERSION, ctx.env.infra);
    reconcile::run(ctx).await;
    Ok(())
}

async fn update() -> Result<()> {
    let http = reqwest::Client::builder().user_agent(concat!("automata/", env!("CARGO_PKG_VERSION"))).build()?;
    let repo = std::env::var("RELEASE_REPO").unwrap_or_else(|_| "bgwastu/automata".into());
    let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
    let Some(update) = selfupdate::check(&http, &repo, token.as_deref()).await? else {
        println!("automata {} is the latest", selfupdate::VERSION);
        return Ok(());
    };
    let exe = selfupdate::install(&http, &update).await?;
    println!("installed automata {} at {}; restart the service to use it", update.version, exe.display());
    Ok(())
}
