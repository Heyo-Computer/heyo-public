//! Peer nodes: the other pg-fc hosts this one is allowed to drive.
//!
//! Everything else in this pooler assumes a single host — `daemon_base_url()`
//! is a loopback URL, the reclaim script runs locally, the run dir is a local
//! path. Replication is the first feature that needs a *second* node, so this
//! module is where the notion of one lives.
//!
//! A peer record carries two independent addresses, and the distinction
//! matters:
//!
//!   * `base_url` + `user`/`password` — the peer's **dashboard**, which is how
//!     this node's control plane drives that one (provision the replica, ask
//!     for its subscription's lag, tell it to promote). There is no separate
//!     peer token: the dashboard's HTTP Basic layer already gates every route,
//!     and adding a second secret to rotate would buy no isolation — the same
//!     reasoning [`crate::dashboard::dedicated`] states for the admin API.
//!   * `pg_host` + `pg_port` — where the peer's **pooler** answers, for a
//!     *guest VM on this host* dialing out. That is the replication data path,
//!     and it is deliberately not derived from `base_url`: the dashboard is
//!     normally a private/loopback listener while the pooler port has to be
//!     routable, and on most deployments they are different addresses
//!     entirely.
//!
//! Format: one `name\tbase_url\tuser\tpassword\tpg_host\tpg_port\tcreated_at`
//! line per record, written atomically (temp file + rename) at mode `0600` —
//! it holds another node's admin password. Validation guarantees no field can
//! contain a tab or a newline, so nothing needs escaping, exactly as in
//! [`crate::dedicated`].
//!
//! A corrupt line is dropped — but, unlike a dedicated credential, it is also
//! journalled to the events page. Losing a credential fails a login, which is
//! loud; losing a *peer* silently orphans a live replication pairing, which is
//! not.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::info;

/// Sanity cap on peer records. Replication is point-to-point and operator-
/// configured; a fleet needing more than this wants a real control plane, not
/// a TSV.
pub const MAX_PEERS: usize = 32;

/// One trusted peer node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Operator-chosen short name; the key. Held to the same shape as a
    /// database name because it is embedded verbatim in replication slot
    /// names, which are narrower than Postgres identifiers.
    pub name: String,
    /// Base URL of the peer's dashboard listener, e.g.
    /// `https://b.example:34199`. Normalized on create: a scheme is required
    /// and any trailing slash is stripped, so joining a path is a plain
    /// concatenation with no double-slash special cases.
    pub base_url: String,
    /// The peer's `PG_VM_POOL_DASHBOARD_USER`.
    pub user: String,
    /// The peer's `PG_VM_POOL_DASHBOARD_PASSWORD`, cleartext — HTTP Basic
    /// transmits it that way and there is nothing to hash against.
    pub password: String,
    /// Host or IPv4 literal a **guest on this host** dials to reach the peer's
    /// `PG_VM_POOL_LISTEN`. Stored as the operator wrote it and resolved to an
    /// IPv4 address host-side before it ever reaches a guest: the microVMs
    /// ship with an empty `/etc/resolv.conf`, which is the same constraint
    /// that makes the S3 path pin IPs with `curl --resolve`.
    pub pg_host: String,
    pub pg_port: u16,
    pub created_at: u64,
}

/// A peer without its dashboard password, for listings that must not carry
/// secrets (the JSON API's `GET`, the dashboard table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub name: String,
    pub base_url: String,
    pub pg_host: String,
    pub pg_port: u16,
    pub created_at: u64,
}

impl Peer {
    fn info(&self) -> PeerInfo {
        PeerInfo {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            pg_host: self.pg_host.clone(),
            pg_port: self.pg_port,
            created_at: self.created_at,
        }
    }

    /// `base_url` with `path` appended. `base_url` is normalized without a
    /// trailing slash on the way in, so this is a plain join.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

/// The peer set, keyed by name.
pub struct PeerStore {
    path: PathBuf,
    by_name: Mutex<HashMap<String, Peer>>,
}

impl PeerStore {
    /// Load from `path`. A missing file starts empty.
    pub fn load(path: PathBuf) -> Self {
        let by_name = match std::fs::read_to_string(&path) {
            Ok(s) => parse(&s),
            Err(_) => HashMap::new(),
        };
        if !by_name.is_empty() {
            info!(
                "loaded {} replication peer(s) from {}",
                by_name.len(),
                path.display()
            );
        }
        Self {
            path,
            by_name: Mutex::new(by_name),
        }
    }

    /// Every peer, password-free, sorted by name for a stable render.
    pub fn list(&self) -> Vec<PeerInfo> {
        let mut out: Vec<_> = self
            .by_name
            .lock()
            .unwrap()
            .values()
            .map(Peer::info)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Option<Peer> {
        self.by_name.lock().unwrap().get(name).cloned()
    }

    /// Record a peer and persist. Validates every field and refuses a name
    /// that is already taken.
    pub fn create(
        &self,
        name: &str,
        base_url: &str,
        user: &str,
        password: &str,
        pg_host: &str,
        pg_port: u16,
    ) -> Result<Peer> {
        let name = crate::dedicated::validate_identifier(name, "peer")?;
        let base_url = validate_base_url(base_url)?;
        let user = validate_field(user, "dashboard user")?;
        let password = validate_field(password, "dashboard password")?;
        let pg_host = validate_host(pg_host)?;
        if pg_port == 0 {
            bail!("pg port must be non-zero");
        }

        let record = Peer {
            name: name.clone(),
            base_url,
            user,
            password,
            pg_host,
            pg_port,
            created_at: now_unix(),
        };
        let snapshot = {
            let mut map = self.by_name.lock().unwrap();
            if map.contains_key(&name) {
                bail!("peer {name:?} already exists");
            }
            if map.len() >= MAX_PEERS {
                bail!("too many peers (max {MAX_PEERS})");
            }
            map.insert(name.clone(), record.clone());
            serialize(&map)
        };
        // Persist before returning, for the same reason a dedicated credential
        // does: the caller is about to act on this peer, and a record that
        // only ever lived in memory would be a pairing the next restart has
        // forgotten while the peer still holds its half.
        if let Err(e) = write_atomic(&self.path, &snapshot) {
            self.by_name.lock().unwrap().remove(&name);
            return Err(e).with_context(|| format!("persisting peers to {}", self.path.display()));
        }
        info!("recorded replication peer {name}");
        Ok(record)
    }

    /// Forget `name` and persist. Returns `false` if it wasn't recorded.
    ///
    /// The caller is responsible for refusing this while a replication record
    /// still names the peer — that check needs the replication store, which
    /// this module deliberately does not know about.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let snapshot = {
            let mut map = self.by_name.lock().unwrap();
            if map.remove(name).is_none() {
                return Ok(false);
            }
            serialize(&map)
        };
        write_atomic(&self.path, &snapshot)
            .with_context(|| format!("persisting peers to {}", self.path.display()))?;
        info!("removed replication peer {name}");
        Ok(true)
    }
}

/// A field that lands in a tab-separated file: printable ASCII, spaces
/// allowed (an operator's Basic-auth password may contain one), but nothing
/// that could break the TSV or a log line.
///
/// Deliberately laxer than [`crate::dedicated::validate_password`]: that one
/// governs a password *this* pooler mints, where a length floor is ours to
/// impose. This one records a password the peer's operator already chose, and
/// refusing a short-but-real credential would just make the feature unusable.
fn validate_field(s: &str, what: &str) -> Result<String> {
    if s.is_empty() {
        bail!("{what} is required");
    }
    if s.len() > 256 {
        bail!("{what} is longer than 256 bytes");
    }
    if !s.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        bail!("{what} must be printable ASCII with no tabs, newlines or control characters");
    }
    Ok(s.to_string())
}

/// Require an absolute `http(s)` URL and strip any trailing slash, so
/// [`Peer::url`] can join paths by concatenation.
fn validate_base_url(s: &str) -> Result<String> {
    let s = s.trim();
    let s = s.strip_suffix('/').unwrap_or(s);
    let s = validate_field(s, "base URL")?;
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        bail!("base URL {s:?} must start with http:// or https://");
    }
    // Nothing beyond the scheme is a hostname, which would make every request
    // fail with a confusing parse error much later.
    let rest = s.split_once("://").map(|(_, r)| r).unwrap_or("");
    if rest.is_empty() || rest.starts_with('/') {
        bail!("base URL {s:?} has no host");
    }
    Ok(s)
}

/// A hostname or IPv4 literal. Narrow on purpose: this value is interpolated
/// into a libpq connection string that a guest runs, so anything that could
/// carry a space, a quote or a second keyword is refused here rather than
/// escaped later.
fn validate_host(s: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() {
        bail!("pg host is required");
    }
    if s.len() > 253 {
        bail!("pg host {s:?} is longer than 253 bytes");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        bail!("pg host {s:?} may contain only letters, digits, dots and dashes");
    }
    Ok(s.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse(s: &str) -> HashMap<String, Peer> {
    let mut map = HashMap::new();
    for line in s.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 6 {
            corrupt("too few fields");
            continue;
        }
        let (name, base_url, user, password, pg_host) = (f[0], f[1], f[2], f[3], f[4]);
        if name.is_empty() || base_url.is_empty() || user.is_empty() || pg_host.is_empty() {
            corrupt("empty field");
            continue;
        }
        let Ok(pg_port) = f[5].parse::<u16>() else {
            corrupt("unparseable pg port");
            continue;
        };
        let created_at = f.get(6).and_then(|v| v.parse().ok()).unwrap_or(0);
        map.insert(
            name.to_string(),
            Peer {
                name: name.to_string(),
                base_url: base_url.to_string(),
                user: user.to_string(),
                password: password.to_string(),
                pg_host: pg_host.to_string(),
                pg_port,
                created_at,
            },
        );
    }
    map
}

/// Both warn and journal. A dropped peer takes a live replication pairing's
/// control path with it, and the events page is where an operator looks for
/// "what broke and when" — a log line alone is too easy to miss.
fn corrupt(why: &str) {
    tracing::warn!("skipping malformed peer line ({why})");
    crate::events::journal_error(
        "replication",
        format!(
            "skipped a malformed line in the peers file ({why}) — a replication pairing may now have no control path"
        ),
    );
}

fn serialize(map: &HashMap<String, Peer>) -> String {
    // Sorted so the file is stable across writes and diffs only when
    // something actually changed.
    let mut records: Vec<_> = map.values().collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = String::new();
    for p in records {
        out.push_str(&p.name);
        out.push('\t');
        out.push_str(&p.base_url);
        out.push('\t');
        out.push_str(&p.user);
        out.push('\t');
        out.push_str(&p.password);
        out.push('\t');
        out.push_str(&p.pg_host);
        out.push('\t');
        out.push_str(&p.pg_port.to_string());
        out.push('\t');
        out.push_str(&p.created_at.to_string());
        out.push('\n');
    }
    out
}

/// Temp file + rename, like [`crate::dedicated`], so a crash mid-write can't
/// leave a truncated file. Mode `0600`: this holds another node's admin
/// password.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    // `mode()` only applies at creation, so an existing temp file from a
    // previous run keeps its old mode — set it unconditionally.
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .with_context(|| format!("tightening permissions on {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PeerStore {
        let path = std::env::temp_dir().join(format!(
            "pgvmpool-peers-{}-{:?}.tsv",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        PeerStore::load(path)
    }

    fn add(s: &PeerStore, name: &str) -> Result<Peer> {
        s.create(
            name,
            "https://b.example:34199",
            "admin",
            "secret",
            "10.0.0.2",
            6432,
        )
    }

    #[test]
    fn create_lookup_and_remove_round_trip_through_disk() {
        let s = store();
        let p = add(&s, "node_b").unwrap();
        assert_eq!(p.pg_port, 6432);
        assert_eq!(s.get("node_b").unwrap().password, "secret");

        let reloaded = PeerStore::load(s.path.clone());
        assert_eq!(reloaded.get("node_b").unwrap(), p);
        // Listings carry no secrets — `PeerInfo` has no password field at all.
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].pg_host, "10.0.0.2");

        assert!(s.remove("node_b").unwrap());
        assert!(!s.remove("node_b").unwrap());
        assert!(PeerStore::load(s.path.clone()).list().is_empty());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn peers_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let s = store();
        add(&s, "node_b").unwrap();
        let mode = std::fs::metadata(&s.path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a peer's dashboard password must not be group/world readable"
        );
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn rejects_duplicates_and_malformed_fields() {
        let s = store();
        add(&s, "node_b").unwrap();
        assert!(add(&s, "node_b").is_err(), "duplicate name");
        // Peer names are Postgres-identifier shaped: they end up inside slot names.
        assert!(add(&s, "Node-B").is_err());
        assert!(add(&s, "9b").is_err());
        // A URL with no scheme would fail much later, at request time.
        assert!(s.create("n1", "b.example:34199", "u", "p", "h", 1).is_err());
        assert!(s.create("n1", "https://", "u", "p", "h", 1).is_err());
        // Nothing that could break the TSV or inject a libpq keyword.
        assert!(s.create("n1", "https://b", "u\tv", "p", "h", 1).is_err());
        assert!(s.create("n1", "https://b", "u", "p\nq", "h", 1).is_err());
        assert!(s.create("n1", "https://b", "u", "p", "h ost", 1).is_err());
        assert!(s.create("n1", "https://b", "u", "p", "h'ost", 1).is_err());
        assert!(
            s.create("n1", "https://b", "u", "p", "host", 0).is_err(),
            "port 0"
        );
        // A short-but-real dashboard password is accepted: the peer's operator
        // chose it, not us.
        assert!(s.create("n1", "https://b", "u", "pw", "host", 5432).is_ok());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn base_url_is_normalized_so_paths_join_by_concatenation() {
        let s = store();
        let p = s
            .create("n", "https://b.example:34199/", "u", "p", "h", 1)
            .unwrap();
        assert_eq!(p.base_url, "https://b.example:34199");
        assert_eq!(
            p.url("/api/databases"),
            "https://b.example:34199/api/databases"
        );
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn parse_skips_corrupt_lines() {
        let map = parse(
            "a\thttps://a\tu\tp\th\t6432\t1700000000\n\
             garbage\n\
             \thttps://b\tu\tp\th\t6432\t1\n\
             c\thttps://c\tu\tp\th\tnotaport\t1\n\
             d\thttps://d\tu\tp\th\t6432\n",
        );
        assert_eq!(map.len(), 2);
        assert_eq!(map["a"].created_at, 1_700_000_000);
        // A missing created_at column (hand-edited file) still loads.
        assert_eq!(map["d"].created_at, 0);
    }

    #[test]
    fn serialize_round_trips_sorted() {
        let map = parse("b\thttps://b\tu\tp2\th\t2\t2\na\thttps://a\tu\tp1\th\t1\t1\n");
        let text = serialize(&map);
        assert!(text.starts_with("a\t"), "{text}");
        let reparsed = parse(&text);
        assert_eq!(reparsed["a"].password, "p1");
        assert_eq!(reparsed["b"].pg_port, 2);
    }
}
