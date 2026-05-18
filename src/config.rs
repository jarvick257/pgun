use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default, rename = "hosts")]
    pub hosts: Vec<Host>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub ssh: String,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub port: u16,
    #[serde(default = "default_scheme")]
    pub scheme: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
}

fn default_scheme() -> String {
    "http".into()
}
fn default_path() -> String {
    "/".into()
}

impl Service {
    pub fn url(&self, local_port: u16) -> String {
        let path = if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };
        format!("{}://127.0.0.1:{}{}", self.scheme, local_port, path)
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no config dir")?;
    Ok(base.join("pgun").join("config.toml"))
}

pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml_edit::de::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    validate(&cfg)?;
    Ok(cfg)
}

pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    validate(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let raw = toml_edit::ser::to_string_pretty(cfg).context("serialize config")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, raw).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for h in &cfg.hosts {
        if h.name.trim().is_empty() {
            bail!("host name empty");
        }
        if !seen.insert(&h.name) {
            bail!("duplicate host name: {}", h.name);
        }
        let mut svc_seen = std::collections::HashSet::new();
        for s in &h.services {
            if s.name.trim().is_empty() {
                bail!("service name empty in host {}", h.name);
            }
            if !svc_seen.insert(&s.name) {
                bail!("duplicate service '{}' in host {}", s.name, h.name);
            }
            if s.port == 0 {
                bail!("invalid port for service '{}'", s.name);
            }
        }
    }
    Ok(())
}
