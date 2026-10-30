use std::net::SocketAddr;

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    /// Deprecated. Imported once into the Discord module of the projects created from Discord
    /// servers (see `upgrade::import_discord_token`), ignored afterwards.
    pub discord_token: Option<String>,
    pub database_url: String,
    pub web_bind: SocketAddr,
    pub cookie_secure: bool,
    pub trust_proxy: bool,
    pub root_password: Option<String>,
    /// 64 hex characters used to encrypt stored API keys. When unset a key file is created
    /// next to the database (see `secrets::Vault::load`).
    pub secrets_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let discord_token = non_empty("DISCORD_TOKEN");
        let database_url =
            non_empty("DATABASE_URL").unwrap_or_else(|| "sqlite://data/scorpio.db?mode=rwc".into());
        let web_bind = non_empty("WEB_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .context("WEB_BIND must be an address like 0.0.0.0:8080")?;

        Ok(Self {
            discord_token,
            database_url,
            web_bind,
            cookie_secure: flag("COOKIE_SECURE"),
            trust_proxy: flag("TRUST_PROXY"),
            root_password: non_empty("ROOT_PASSWORD"),
            secrets_key: non_empty("SECRETS_KEY"),
        })
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn flag(key: &str) -> bool {
    matches!(
        non_empty(key)
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}
