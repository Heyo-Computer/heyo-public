//! Shared axum application state: the pooler registry plus dashboard settings.

use std::sync::Arc;

use crate::config::DashboardConfig;
use crate::registry::SchemaRegistry;

use super::alerts::AlertStore;
use super::history::VmHistory;
use super::model::InventoryCache;

/// Cloned per request by axum; every field is an `Arc` so cloning is cheap. The
/// shared `alerts` store is read by the monitoring page and mutated by its
/// add/delete actions and the background evaluator; `history` is appended by the
/// background sampler and read by the monitoring chart; `inventory` is the
/// stale-while-revalidate daemon-inventory cache every page render reads.
#[derive(Clone)]
pub struct DashState {
    pub registry: Arc<SchemaRegistry>,
    pub cfg: Arc<DashboardConfig>,
    pub alerts: Arc<AlertStore>,
    pub history: Arc<VmHistory>,
    pub inventory: Arc<InventoryCache>,
}
