use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use config::{Config as ConfigBuilder, Environment, File};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default)]
    pub database_url: String,
    #[serde(default)]
    pub internal_api_key: String,
    #[serde(default)]
    pub master_key: String,
    /// Admin password gating the web dashboard. When empty the dashboard is
    /// disabled (login always fails) — the machine `/v1/secrets/*` API is
    /// unaffected.
    #[serde(default)]
    pub admin_password: String,
    /// Whether the dashboard session cookie sets the `Secure` attribute.
    /// Leave false for local http; set true behind TLS in production.
    #[serde(default)]
    pub cookie_secure: bool,
    /// Dashboard session lifetime in seconds.
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
    #[serde(default = "default_db_min_connections")]
    pub db_min_connections: u32,
    #[serde(default = "default_db_acquire_timeout_seconds")]
    pub db_acquire_timeout_seconds: u64,
    #[serde(default = "default_db_idle_timeout_seconds")]
    pub db_idle_timeout_seconds: u64,
    #[serde(default = "default_db_max_lifetime_seconds")]
    pub db_max_lifetime_seconds: u64,
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut builder = ConfigBuilder::builder()
            .set_default("server_port", default_server_port())?
            .set_default("database_url", "")?
            .set_default("internal_api_key", "")?
            .set_default("master_key", "")?
            .set_default("admin_password", "")?
            .set_default("cookie_secure", false)?
            .set_default("session_ttl_seconds", default_session_ttl_seconds())?
            .set_default("db_max_connections", default_db_max_connections())?
            .set_default("db_min_connections", default_db_min_connections())?
            .set_default("db_acquire_timeout_seconds", default_db_acquire_timeout_seconds())?
            .set_default("db_idle_timeout_seconds", default_db_idle_timeout_seconds())?
            .set_default("db_max_lifetime_seconds", default_db_max_lifetime_seconds())?;

        if let Some(path) = Self::get_config_path() {
            if path.exists() {
                builder = builder.add_source(File::from(path));
            }
        }

        builder = builder.add_source(
            Environment::with_prefix("HEYOSECRET")
                .separator("_")
                .try_parsing(true),
        );

        let mut config: Config = builder.build()?.try_deserialize()?;

        if let Ok(value) = env::var("HEYOSECRET_SERVER_PORT") {
            if !value.trim().is_empty() {
                config.server_port = value.parse()?;
            }
        }
        if config.database_url.is_empty() {
            config.database_url = env::var("HEYOSECRET_DATABASE_URL")
                .or_else(|_| env::var("DATABASE_URL"))
                .unwrap_or_default();
        }
        if config.internal_api_key.is_empty() {
            config.internal_api_key = env::var("HEYOSECRET_INTERNAL_API_KEY")
                .or_else(|_| env::var("PLATFORM_INTERNAL_API_KEY"))
                .unwrap_or_default();
        }
        if config.master_key.is_empty() {
            config.master_key = env::var("HEYOSECRET_MASTER_KEY").unwrap_or_default();
        }
        if config.admin_password.is_empty() {
            config.admin_password = env::var("HEYOSECRET_ADMIN_PASSWORD").unwrap_or_default();
        }

        if config.database_url.is_empty() {
            return Err(anyhow!(
                "DATABASE_URL is required (or HEYOSECRET_DATABASE_URL)"
            ));
        }
        if config.internal_api_key.trim().is_empty() {
            return Err(anyhow!(
                "HEYOSECRET_INTERNAL_API_KEY is required for HeyoSecret API access"
            ));
        }
        if config.master_key.trim().is_empty() {
            return Err(anyhow!(
                "HEYOSECRET_MASTER_KEY is required to encrypt secret values"
            ));
        }
        if config.master_key.as_bytes().len() < 32 {
            return Err(anyhow!(
                "HEYOSECRET_MASTER_KEY must be at least 32 bytes before key derivation"
            ));
        }
        if config.db_max_connections == 0 {
            config.db_max_connections = default_db_max_connections();
        }
        if config.db_min_connections > config.db_max_connections {
            config.db_min_connections = config.db_max_connections;
        }

        Ok(config)
    }

    pub fn encryption_key(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.master_key.as_bytes());
        hasher.update(b"heyo-secret-v1");
        hasher.finalize().into()
    }

    /// Key used to sign dashboard session cookies. Derived from the master key
    /// with a distinct domain-separation tag so it never overlaps the value
    /// encryption key.
    pub fn session_signing_key(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.master_key.as_bytes());
        hasher.update(self.admin_password.as_bytes());
        hasher.update(b"heyo-secret-session-v1");
        hasher.finalize().into()
    }

    /// The dashboard is only reachable when an admin password is configured.
    pub fn dashboard_enabled(&self) -> bool {
        !self.admin_password.trim().is_empty()
    }

    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_secs(self.db_acquire_timeout_seconds)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.db_idle_timeout_seconds)
    }

    pub fn max_lifetime(&self) -> Duration {
        Duration::from_secs(self.db_max_lifetime_seconds)
    }

    fn get_config_path() -> Option<PathBuf> {
        if let Ok(path) = env::var("HEYOSECRET_CONFIG_PATH") {
            return Some(PathBuf::from(path));
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("heyosecret.toml");
        path.exists().then_some(path)
    }
}

pub fn resolve_migrations_dir() -> Result<PathBuf> {
    if let Ok(dir) = env::var("HEYOSECRET_MIGRATIONS_DIR") {
        let path = PathBuf::from(dir);
        if path.exists() {
            return Ok(path);
        }
    }

    let cwd_migrations = Path::new("migrations");
    if cwd_migrations.exists() {
        return Ok(cwd_migrations.to_path_buf());
    }

    let compile_time = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    if compile_time.exists() {
        return Ok(compile_time);
    }

    Err(anyhow!("No HeyoSecret migrations directory found")).context("resolve migrations")
}

fn default_server_port() -> u16 {
    4455
}

fn default_session_ttl_seconds() -> u64 {
    43_200 // 12 hours
}

fn default_db_max_connections() -> u32 {
    20
}

fn default_db_min_connections() -> u32 {
    2
}

fn default_db_acquire_timeout_seconds() -> u64 {
    30
}

fn default_db_idle_timeout_seconds() -> u64 {
    600
}

fn default_db_max_lifetime_seconds() -> u64 {
    1800
}
