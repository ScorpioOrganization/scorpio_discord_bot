//! Calls [`Module::on_tick`] about once a minute for every guild a ticking module is
//! enabled in. A slow or failing guild never blocks the others or stops the loop.

use std::time::Duration;

use serenity::all::GuildId;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::modules;
use crate::state::AppState;

pub const TICK: Duration = Duration::from_secs(60);
/// Upper bound for one module's work in one guild.
const TICK_TIMEOUT: Duration = Duration::from_secs(50);

pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        tick_once(&state).await;
    }
}

/// One round: every ticking module, every enabled guild the bot is in, concurrently.
pub async fn tick_once(state: &AppState) {
    let Some(discord) = state.discord() else {
        return;
    };
    let mut tasks = JoinSet::new();
    for module in state.modules.iter().filter(|m| m.ticks()) {
        let guilds = match modules::enabled_guilds(&state.db, module.id()).await {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(module = module.id(), "could not list enabled guilds: {e:#}");
                continue;
            }
        };
        for guild_id in guilds {
            if guild_id == 0 || discord.cache.guild(GuildId::new(guild_id)).is_none() {
                continue;
            }
            let (module, state, discord) = (module.clone(), state.clone(), discord.clone());
            tasks.spawn(async move {
                let work = module.on_tick(&state, &discord, guild_id);
                match tokio::time::timeout(TICK_TIMEOUT, work).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::error!(module = module.id(), guild_id, "tick failed: {e:#}")
                    }
                    Err(_) => tracing::warn!(module = module.id(), guild_id, "tick timed out"),
                }
            });
        }
    }
    while let Some(res) = tasks.join_next().await {
        if let Err(e) = res {
            tracing::error!("tick task panicked: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use axum::Router;

    use super::*;
    use crate::modules::{Module, StatItem, set_enabled};
    use crate::state::DiscordHandle;
    use crate::test_util;

    #[derive(Default)]
    struct Recorder {
        ticks: Mutex<Vec<u64>>,
        ticking: bool,
    }

    struct Probe(Arc<Recorder>, &'static str);

    #[async_trait]
    impl Module for Probe {
        fn id(&self) -> &'static str {
            self.1
        }
        fn name(&self) -> &'static str {
            "Probe"
        }
        fn description(&self) -> &'static str {
            ""
        }
        async fn summary(&self, _: &AppState, _: u64) -> Result<Vec<StatItem>> {
            Ok(Vec::new())
        }
        fn routes(&self) -> Router<AppState> {
            Router::new()
        }
        fn ticks(&self) -> bool {
            self.0.ticking
        }
        async fn on_tick(&self, _: &AppState, _: &DiscordHandle, guild_id: u64) -> Result<()> {
            self.0.ticks.lock().unwrap().push(guild_id);
            if guild_id == test_util::GUILD_ID {
                anyhow::bail!("errors are logged, not fatal");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn ticks_enabled_guilds_the_bot_is_in() {
        let ticking = Arc::new(Recorder {
            ticking: true,
            ..Default::default()
        });
        let silent = Arc::new(Recorder::default());
        let modules: Vec<Arc<dyn Module>> = vec![
            Arc::new(Probe(ticking.clone(), "probe")),
            Arc::new(Probe(silent.clone(), "silent")),
        ];
        let state = test_util::state_with(modules).await;

        // Not connected: nothing runs.
        set_enabled(&state.db, test_util::GUILD_ID, "probe", true)
            .await
            .unwrap();
        tick_once(&state).await;
        assert!(ticking.ticks.lock().unwrap().is_empty());

        test_util::connect_fake_discord(&state);
        let other = test_util::GUILD_ID + 1;
        set_enabled(&state.db, other, "probe", true).await.unwrap();
        set_enabled(&state.db, test_util::GUILD_ID, "silent", true)
            .await
            .unwrap();
        tick_once(&state).await;
        tick_once(&state).await;

        // The bot is not in `other`, and `silent` does not tick.
        assert_eq!(
            *ticking.ticks.lock().unwrap(),
            [test_util::GUILD_ID, test_util::GUILD_ID]
        );
        assert!(silent.ticks.lock().unwrap().is_empty());
    }
}
