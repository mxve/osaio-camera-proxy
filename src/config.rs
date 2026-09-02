use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct OsaioConfig {
    pub appid: String,
    pub app_secret: String,
    pub user_agent: String,
    pub global_base_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub osaio: OsaioConfig,
    pub account: AccountConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file at {:?}", path.as_ref()))?;
        toml::from_str(&content)
            .with_context(|| format!("failed to parse config file at {:?}", path.as_ref()))
    }
}
