//! Database module - handles PostgreSQL connections for the orchestrator.

use anyhow::Result;
use once_cell::sync::OnceCell;
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;

static DB_CONNECTION: OnceCell<DatabaseConnection> = OnceCell::new();

fn resolve_migrations_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ORCHESTRATOR_MIGRATIONS_DIR") {
        let path = PathBuf::from(dir);
        if path.exists() {
            return Some(path);
        }
    }

    let cwd_migrations = Path::new("migrations");
    if cwd_migrations.exists() {
        return Some(cwd_migrations.to_path_buf());
    }

    let compile_time = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    if compile_time.exists() {
        return Some(compile_time);
    }

    None
}

async fn run_migrations(db: &DatabaseConnection) -> Result<()> {
    let migrations_dir = match resolve_migrations_dir() {
        Some(dir) => dir,
        None => {
            tracing::warn!("No orchestrator migrations directory found; skipping migrations");
            return Ok(());
        }
    };

    tracing::info!(
        "Running orchestrator migrations from: {}",
        migrations_dir.display()
    );

    let mut files: Vec<_> = std::fs::read_dir(&migrations_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort_by_key(|entry| entry.file_name());

    for entry in files {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy();
        let sql = std::fs::read_to_string(&path)?;
        let sql = sql.trim();
        if sql.is_empty() {
            continue;
        }

        match db.execute_unprepared(sql).await {
            Ok(_) => tracing::info!("Orchestrator migration {} applied", name),
            Err(error) => tracing::warn!(
                "Orchestrator migration {} encountered error (may already exist): {}",
                name,
                error
            ),
        }
    }

    Ok(())
}

pub async fn init_database(config: &Config) -> Result<()> {
    let mut options = ConnectOptions::new(config.database_url.to_owned());
    options
        .max_connections(config.db_max_connections)
        .min_connections(config.db_min_connections)
        .connect_timeout(Duration::from_secs(config.db_connect_timeout_seconds))
        .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_seconds))
        .idle_timeout(Duration::from_secs(config.db_idle_timeout_seconds))
        .max_lifetime(Duration::from_secs(config.db_max_lifetime_seconds))
        .sqlx_logging(true);

    let db = Database::connect(options).await?;
    run_migrations(&db).await?;

    DB_CONNECTION
        .set(db)
        .map_err(|_| anyhow::anyhow!("Database already initialized"))?;

    tracing::info!(
        max_connections = config.db_max_connections,
        min_connections = config.db_min_connections,
        acquire_timeout_seconds = config.db_acquire_timeout_seconds,
        idle_timeout_seconds = config.db_idle_timeout_seconds,
        max_lifetime_seconds = config.db_max_lifetime_seconds,
        "Orchestrator database connection pool initialized"
    );
    Ok(())
}

pub fn get_db() -> Result<&'static DatabaseConnection> {
    DB_CONNECTION
        .get()
        .ok_or_else(|| anyhow::anyhow!("Database not initialized"))
}
