use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub(crate) config: Arc<Config>,
    pub http_client: reqwest::Client,
    pub worker_id: Arc<String>,
    pub ci_workspace_cache: Arc<Mutex<HashMap<String, PathBuf>>>,
}
