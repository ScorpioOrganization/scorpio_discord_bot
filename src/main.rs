mod auth;
mod bot;
mod config;
mod db;
mod error;
mod modules;
mod state;
#[cfg(test)]
mod test_util;
mod web;

use std::time::Duration;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::{AppState, DiscordHandle};

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

    let state = AppState::new(db, config.clone(), modules::registry());
    tokio::spawn(maintenance(state.clone()));

    let bot_task = match config.discord_token.as_deref() {
        Some(token) => match bot::build_client(token, state.clone()).await {
            Ok((mut client, intents)) => {
                let _ = state.discord.set(DiscordHandle {
                    http: client.http.clone(),
                    cache: client.cache.clone(),
                    shard_manager: client.shard_manager.clone(),
                    intents,
                });
                let task_state = state.clone();
                Some(tokio::spawn(async move {
                    if let Err(e) = client.start().await {
                        let reason = bot::describe_stop(&e);
                        tracing::error!("{reason}");
                        task_state.set_bot_error(reason);
                    }
                }))
            }
            Err(e) => {
                // Keep the dashboard up so the problem is visible there instead of a restart loop.
                tracing::error!("Discord bot not started: {e:#}");
                state.set_bot_error(format!("{e:#}"));
                None
            }
        },
        None => {
            tracing::warn!("DISCORD_TOKEN is not set, running the dashboard only");
            None
        }
    };

    web::serve(state.clone(), shutdown_signal()).await?;

    // Shards of a client that already stopped cannot be told to shut down.
    if let (Some(discord), Some(task)) = (state.discord(), &bot_task)
        && !task.is_finished()
    {
        discord.shard_manager.shutdown_all().await;
    }
    tracing::info!("bye");
    Ok(())
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
        state.limiter.prune();
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
