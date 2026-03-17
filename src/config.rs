use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub user: HashMap<String, UserConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    pub host_key: Option<String>,
    pub host_key_rsa: Option<String>,
    pub header: Option<String>,
    pub footer: Option<String>,
}

fn default_port() -> u16 {
    4242
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserConfig {
    pub castfile: String,
    pub header: Option<String>,
    pub footer: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        toml::from_str(&content).context("failed to parse config file")
    }

    pub fn user_config(&self, username: &str) -> Option<&UserConfig> {
        self.user.get(username)
        .or_else(|| self.user.get("default"))
    }
}
