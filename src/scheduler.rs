//! Calls [`Module::on_tick`] about once a minute for every project a ticking module is
//! enabled in. A slow or failing project never blocks the others or stops the loop.
//!
//! [`Module::on_tick`]: crate::modules::Module::on_tick

use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::modules;
use crate::state::AppState;

pub const TICK: Duration = Duration::from_secs(60);
/// Upper bound for one module's work in one project.
const TICK_TIMEOUT: Duration = Duration::from_secs(50);

pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        tick_once(&state).await;
    }
}

/// One round: every ticking module, every project it is enabled in, concurrently.
pub async fn tick_once(state: &AppState) {
    let mut tasks = JoinSet::new();
    for module in state.modules.iter().filter(|m| m.ticks()) {
        let projects = match modules::enabled_projects(&state.db, module.id()).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    module = module.id(),
                    "could not list enabled projects: {e:#}"
                );
                continue;
            }
        };
        for project_id in projects {
            let (module, state) = (module.clone(), state.clone());
            tasks.spawn(async move {
                let work = module.on_tick(&state, project_id);
                match tokio::time::timeout(TICK_TIMEOUT, work).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::error!(module = module.id(), project_id, "tick failed: {e:#}")
                    }
                    Err(_) => tracing::warn!(module = module.id(), project_id, "tick timed out"),
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
    use crate::test_util;

    #[derive(Default)]
    struct Recorder {
        ticks: Mutex<Vec<i64>>,
        ticking: bool,
        failing: Mutex<Option<i64>>,
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
        async fn summary(&self, _: &AppState, _: i64) -> Result<Vec<StatItem>> {
            Ok(Vec::new())
        }
        fn routes(&self) -> Router<AppState> {
            Router::new()
        }
        fn ticks(&self) -> bool {
            self.0.ticking
        }
        async fn on_tick(&self, _: &AppState, project_id: i64) -> Result<()> {
            self.0.ticks.lock().unwrap().push(project_id);
            if *self.0.failing.lock().unwrap() == Some(project_id) {
                anyhow::bail!("errors are logged, not fatal");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn ticks_enabled_projects_without_discord() {
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
        let [a, b, c] = test_util::projects(&state.db, ["A", "B", "C"]).await;

        set_enabled(&state.db, a, "probe", true).await.unwrap();
        set_enabled(&state.db, b, "probe", true).await.unwrap();
        set_enabled(&state.db, c, "silent", true).await.unwrap();
        *ticking.failing.lock().unwrap() = Some(a);
        tick_once(&state).await;
        tick_once(&state).await;

        // A failing project does not stop the others; `silent` does not tick.
        let mut ticks = ticking.ticks.lock().unwrap().clone();
        ticks.sort();
        assert_eq!(ticks, [a, a, b, b]);
        assert!(silent.ticks.lock().unwrap().is_empty());
    }
}
