mod auth;
mod cache;
mod clickup;
mod config;
mod db;
mod discord;
mod error;
mod keys;
mod modules;
mod projects;
mod scheduler;
mod secrets;
mod state;
#[cfg(test)]
mod test_util;
mod upgrade;
mod web;

use std::time::Duration;

use anyhow::Result;
use sqlx::SqlitePool;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,serenity=warn,tracing::span=warn")),
        )
        .init();

    let config = Config::from_env()?;
    let db = db::connect(&config.database_url).await?;

    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => {}
        Some("reset-root") => {
            let password = auth::reset_root_password(&db).await?;
            println!("New root password: {password}");
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown command `{other}`. Available: serve, reset-root"),
    }

    if let Some(password) = auth::bootstrap_root(&db, config.root_password.as_deref()).await? {
        tracing::warn!(
            "created dashboard account `{}` with password: {password}  (shown only once, change it after logging in)",
            auth::ROOT_USERNAME
        );
    }
    if !config.cookie_secure {
        tracing::warn!("COOKIE_SECURE is off. Turn it on when the dashboard is served over HTTPS.");
    }

    let state = prepare(config, db).await?;
    tokio::spawn(maintenance(state.clone()));
    tokio::spawn(discord::hub::DiscordHub::run(state.clone()));
    tokio::spawn(scheduler::run(state.clone()));

    web::serve(state.clone(), shutdown_signal()).await?;

    state.discord.shutdown_all().await;
    tracing::info!("bye");
    Ok(())
}

/// Everything between opening the database and serving: the vault, the state and the data
/// upgrades that follow migrations.
async fn prepare(config: Config, db: SqlitePool) -> Result<AppState> {
    let vault = secrets::Vault::load(&config)?;
    let state = AppState::new(db, config, modules::registry(), vault);
    upgrade::run(&state).await;
    Ok(state)
}

/// Hourly cleanup of expired sessions, old log rows and stale rate limit entries.
async fn maintenance(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        if let Err(e) = auth::delete_expired_sessions(&state.db).await {
            tracing::error!("session cleanup failed: {e:#}");
        }
        if let Err(e) = modules::greeting::prune_log(&state.db).await {
            tracing::error!("greeting log cleanup failed: {e:#}");
        }
        if let Err(e) = modules::calendar::prune(&state.db).await {
            tracing::error!("calendar log cleanup failed: {e:#}");
        }
        if let Err(e) = modules::recurring::prune(&state.db).await {
            tracing::error!("recurring tasks log cleanup failed: {e:#}");
        }
        state.limiter.prune();
        state.members.prune();
        state.cache.prune(Duration::from_secs(3600));
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
