//! Pull-based sources, as opposed to the push paths in `ingest`.

pub mod applb;
pub mod heyvm;

/// One live VM backend and the deployment it serves, as reported by app-lb's
/// metrics snapshot. The poller publishes the current set after every
/// successful poll; the daemon log tailer keeps one stream per entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmTarget {
    pub deployment: String,
    /// The sandbox id, as the daemon knows it.
    pub backend: String,
}
