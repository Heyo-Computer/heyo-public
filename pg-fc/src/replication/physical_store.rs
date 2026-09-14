//! Durable ownership records for physical-streaming replacement candidates.
//!
//! This is deliberately separate from the logical replication store. A row is
//! never deleted or made non-owning by an error: until cleanup is implemented,
//! every recorded operation reserves its database and any VM IDs it names.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalPhase {
    Intent,
    Creating,
    Candidate,
    Seeding,
    Verified,
    Activated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalRecord {
    pub database: String,
    pub generation: String,
    pub candidate_name: String,
    pub candidate_id: Option<String>,
    pub previous_vm_id: Option<String>,
    pub source_node: String,
    pub source_vm_id: String,
    pub system_identifier: String,
    pub pg_major: u32,
    pub slot: String,
    pub phase: PhysicalPhase,
    pub last_error: Option<String>,
}

impl PhysicalRecord {
    pub fn candidate_name(generation: &str) -> String {
        format!("repl-seed-{generation}")
    }
}

pub struct PhysicalStore {
    path: PathBuf,
    by_database: Mutex<HashMap<String, PhysicalRecord>>,
}

/// Source-side ownership of the physical slot.  This is intentionally not a
/// `PhysicalRecord`: a source is never a candidate and must not accidentally
/// participate in candidate activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalSourceRecord {
    pub database: String,
    pub generation: String,
    pub source_vm_id: String,
    pub system_identifier: String,
    pub pg_major: u32,
    pub slot: String,
    pub source_lsn: String,
    pub peer: String,
    pub last_error: Option<String>,
}

pub struct PhysicalSourceStore {
    path: PathBuf,
    by_database: Mutex<HashMap<String, PhysicalSourceRecord>>,
}

impl PhysicalSourceStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let records: Vec<PhysicalSourceRecord> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing physical source records at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading physical source records at {}", path.display())),
        };
        let mut by_database = HashMap::new();
        for record in records {
            validate_source(&record)?;
            if by_database.insert(record.database.clone(), record).is_some() {
                bail!("duplicate physical source record for a database");
            }
        }
        Ok(Self { path, by_database: Mutex::new(by_database) })
    }

    pub fn get(&self, database: &str) -> Option<PhysicalSourceRecord> {
        self.by_database.lock().unwrap().get(database).cloned()
    }

    pub fn owns_vm(&self, id: &str) -> bool {
        self.by_database.lock().unwrap().values().any(|r| r.source_vm_id == id)
    }

    pub fn create(&self, record: PhysicalSourceRecord) -> Result<PhysicalSourceRecord> {
        validate_source(&record)?;
        let mut records = self.by_database.lock().unwrap();
        if let Some(existing) = records.get(&record.database) {
            let mut observed = record.clone();
            observed.source_lsn = existing.source_lsn.clone();
            observed.last_error = existing.last_error.clone();
            if existing == &observed { return Ok(existing.clone()); }
            bail!("database {:?} already has a physical source operation", record.database);
        }
        let mut next = records.clone();
        next.insert(record.database.clone(), record.clone());
        persist_values(&self.path, next.values())?;
        *records = next;
        Ok(record)
    }
}

impl PhysicalStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let records: Vec<PhysicalRecord> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing physical replication records at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading physical replication records at {}", path.display())),
        };
        let mut by_database: HashMap<String, PhysicalRecord> = HashMap::with_capacity(records.len());
        for record in records {
            validate_record(&record).with_context(|| format!("invalid physical replication record for {:?}", record.database))?;
            if by_database.values().any(|r| r.candidate_name == record.candidate_name
                || r.candidate_id.is_some() && r.candidate_id == record.candidate_id) {
                bail!("multiple physical records claim the same candidate");
            }
            if by_database.insert(record.database.clone(), record).is_some() {
                bail!("duplicate physical replication record for a database");
            }
        }
        Ok(Self { path, by_database: Mutex::new(by_database) })
    }

    pub fn list(&self) -> Vec<PhysicalRecord> {
        let mut records: Vec<_> = self.by_database.lock().unwrap().values().cloned().collect();
        records.sort_by(|a, b| a.database.cmp(&b.database));
        records
    }

    pub fn get(&self, database: &str) -> Option<PhysicalRecord> {
        self.by_database.lock().unwrap().get(database).cloned()
    }

    pub fn create(&self, record: PhysicalRecord) -> Result<PhysicalRecord> {
        validate_record(&record)?;
        if record.phase != PhysicalPhase::Intent || record.candidate_id.is_some() {
            bail!("a new physical replication operation must start at intent without a candidate VM ID");
        }
        let mut records = self.by_database.lock().unwrap();
        if let Some(existing) = records.get(&record.database) {
            let mut intent = existing.clone();
            intent.phase = PhysicalPhase::Intent;
            intent.candidate_id = None;
            intent.last_error = None;
            if intent == record { return Ok(existing.clone()); }
            bail!("database {:?} already has a physical replication operation", record.database);
        }
        if records.values().any(|r| r.candidate_name == record.candidate_name) {
            bail!("physical generation already names another database's candidate");
        }
        let mut next = records.clone();
        next.insert(record.database.clone(), record.clone());
        persist(&self.path, &next)?;
        *records = next;
        Ok(record)
    }

    pub fn advance(
        &self,
        database: &str,
        generation: &str,
        expected_phase: PhysicalPhase,
        next_phase: PhysicalPhase,
        candidate_id: Option<String>,
    ) -> Result<PhysicalRecord> {
        let mut records = self.by_database.lock().unwrap();
        let current = records.get(database).with_context(|| format!("no physical replication operation for database {database:?}"))?;
        if current.generation != generation || current.phase != expected_phase {
            bail!("stale physical replication update for database {database:?}: generation or phase no longer matches");
        }
        if phase_number(next_phase) != phase_number(expected_phase) + 1 {
            bail!("illegal physical replication phase transition from {expected_phase:?} to {next_phase:?}");
        }
        let mut updated = current.clone();
        if let Some(id) = candidate_id {
            validate_token("candidate VM ID", &id, 128)?;
            if let Some(bound) = &updated.candidate_id {
                if bound != &id { bail!("candidate VM ID is already bound and cannot be changed"); }
            } else {
                updated.candidate_id = Some(id);
            }
        }
        updated.phase = next_phase;
        updated.last_error = None;
        validate_record(&updated)?;
        let mut next = records.clone();
        next.insert(database.to_string(), updated.clone());
        persist(&self.path, &next)?;
        *records = next;
        Ok(updated)
    }

    pub fn set_error(&self, database: &str, generation: &str, error: Option<String>) -> Result<PhysicalRecord> {
        let mut records = self.by_database.lock().unwrap();
        let current = records.get(database).with_context(|| format!("no physical replication operation for database {database:?}"))?;
        if current.generation != generation { bail!("stale physical replication error update for database {database:?}"); }
        if let Some(message) = &error { validate_error(message)?; }
        let mut updated = current.clone();
        updated.last_error = error;
        let mut next = records.clone();
        next.insert(database.to_string(), updated.clone());
        persist(&self.path, &next)?;
        *records = next;
        Ok(updated)
    }

    pub fn owns_vm(&self, id: &str) -> bool {
        self.by_database.lock().unwrap().values().any(|r| {
            r.candidate_id.as_deref() == Some(id) || r.previous_vm_id.as_deref() == Some(id)
        })
    }

    pub fn reserves_database(&self, database: &str) -> bool {
        self.by_database.lock().unwrap().contains_key(database)
    }
}

fn phase_number(phase: PhysicalPhase) -> u8 {
    match phase {
        PhysicalPhase::Intent => 0,
        PhysicalPhase::Creating => 1,
        PhysicalPhase::Candidate => 2,
        PhysicalPhase::Seeding => 3,
        PhysicalPhase::Verified => 4,
        PhysicalPhase::Activated => 5,
    }
}

fn validate_record(record: &PhysicalRecord) -> Result<()> {
    validate_pg_identifier("database", &record.database)?;
    validate_generation(&record.generation)?;
    let derived = PhysicalRecord::candidate_name(&record.generation);
    if record.candidate_name != derived { bail!("candidate name must be derived from the generation"); }
    validate_token("source node", &record.source_node, 128)?;
    validate_token("source VM ID", &record.source_vm_id, 128)?;
    if let Some(id) = &record.candidate_id { validate_token("candidate VM ID", id, 128)?; }
    if let Some(id) = &record.previous_vm_id { validate_token("previous VM ID", id, 128)?; }
    if record.candidate_id.as_ref().is_some_and(|id| Some(id) == record.previous_vm_id.as_ref()) {
        bail!("candidate VM ID must differ from the previous VM ID");
    }
    if record.system_identifier.is_empty() || record.system_identifier.len() > 20 || !record.system_identifier.bytes().all(|b| b.is_ascii_digit()) {
        bail!("PostgreSQL system identifier must be a numeric string");
    }
    if record.pg_major == 0 { bail!("PostgreSQL major version must be nonzero"); }
    validate_pg_identifier("replication slot", &record.slot)?;
    if phase_number(record.phase) >= phase_number(PhysicalPhase::Candidate) && record.candidate_id.is_none() {
        bail!("phase {:?} requires a candidate VM ID", record.phase);
    }
    if matches!(record.phase, PhysicalPhase::Intent | PhysicalPhase::Creating) && record.candidate_id.is_some() {
        bail!("intent cannot already bind a candidate VM");
    }
    if let Some(error) = &record.last_error { validate_error(error)?; }
    Ok(())
}

fn validate_source(record: &PhysicalSourceRecord) -> Result<()> {
    validate_pg_identifier("database", &record.database)?;
    validate_generation(&record.generation)?;
    validate_token("source VM ID", &record.source_vm_id, 128)?;
    validate_token("peer", &record.peer, 128)?;
    validate_pg_identifier("replication slot", &record.slot)?;
    if record.system_identifier.is_empty() || record.system_identifier.len() > 20 || !record.system_identifier.bytes().all(|b| b.is_ascii_digit()) { bail!("PostgreSQL system identifier must be a numeric string"); }
    if record.pg_major == 0 || record.source_lsn.is_empty() || record.source_lsn.len() > 32 { bail!("invalid physical source identity"); }
    if let Some(error) = &record.last_error { validate_error(error)?; }
    Ok(())
}

fn validate_generation(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 52 || !value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || !value.as_bytes()[0].is_ascii_lowercase() && !value.as_bytes()[0].is_ascii_digit()
        || value.ends_with('-')
    { bail!("generation must be a safe lowercase identifier"); }
    Ok(())
}

fn validate_pg_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 63 || !value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || !value.as_bytes()[0].is_ascii_lowercase()
    { bail!("{label} must be a safe PostgreSQL identifier"); }
    Ok(())
}

fn validate_token(label: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')) {
        bail!("{label} contains unsafe characters");
    }
    Ok(())
}

fn validate_error(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 500 || value.chars().any(|c| c.is_control()) {
        bail!("last error must be a nonempty printable message of at most 500 bytes");
    }
    Ok(())
}

fn persist(path: &Path, records: &HashMap<String, PhysicalRecord>) -> Result<()> {
    persist_values(path, records.values())
}

fn persist_values<'a, T: Serialize + 'a>(path: &Path, records: impl Iterator<Item = &'a T>) -> Result<()> {
    let ordered: Vec<_> = records.collect();
    let bytes = serde_json::to_vec_pretty(&ordered).context("serializing physical replication records")?;
    let tmp = path.with_extension("physical.json.tmp");
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
            .with_context(|| format!("opening temporary physical replication file beside {}", path.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming physical replication records into {}", path.display()))?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)?.sync_all().context("fsync physical replication records directory")
    })();
    if result.is_err() { let _ = std::fs::remove_file(tmp); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pgfc-physical-{label}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }
    fn record(database: &str, generation: &str) -> PhysicalRecord {
        PhysicalRecord { database: database.into(), generation: generation.into(), candidate_name: PhysicalRecord::candidate_name(generation), candidate_id: None, previous_vm_id: Some("logical-vm-1".into()), source_node: "eu2".into(), source_vm_id: "source-vm-1".into(), system_identifier: "7431234567890123456".into(), pg_major: 17, slot: "physical_acme".into(), phase: PhysicalPhase::Intent, last_error: None }
    }
    #[test]
    fn roundtrip_is_private() {
        let p = path("roundtrip");
        let store = PhysicalStore::load(p.clone()).unwrap();
        store.create(record("acme", "g1")).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(PhysicalStore::load(p.clone()).unwrap().list(), store.list());
        let _ = std::fs::remove_file(p);
    }
    #[test]
    fn malformed_load_fails() {
        let p = path("malformed"); std::fs::write(&p, b"not json").unwrap();
        assert!(PhysicalStore::load(p.clone()).is_err()); let _ = std::fs::remove_file(p);
    }
    #[test]
    fn failed_persistence_does_not_change_memory() {
        let p = path("missing").join("records.json");
        let store = PhysicalStore::load(p).unwrap();
        assert!(store.create(record("acme", "g1")).is_err());
        assert!(!store.reserves_database("acme"));
    }
    #[test]
    fn stale_and_reordered_advances_fail() {
        let p = path("cas"); let store = PhysicalStore::load(p.clone()).unwrap(); store.create(record("acme", "g1")).unwrap();
        assert!(store.advance("acme", "old", PhysicalPhase::Intent, PhysicalPhase::Candidate, None).is_err());
        assert!(store.advance("acme", "g1", PhysicalPhase::Intent, PhysicalPhase::Seeding, Some("candidate-1".into())).is_err());
        store.advance("acme", "g1", PhysicalPhase::Intent, PhysicalPhase::Creating, None).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Creating, PhysicalPhase::Candidate, Some("candidate-1".into())).unwrap();
        assert!(store.advance("acme", "g1", PhysicalPhase::Intent, PhysicalPhase::Candidate, None).is_err()); let _ = std::fs::remove_file(p);
    }
    #[test]
    fn ownership_survives_errors_and_restart() {
        let p = path("owner"); let store = PhysicalStore::load(p.clone()).unwrap(); store.create(record("acme", "g1")).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Intent, PhysicalPhase::Creating, None).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Creating, PhysicalPhase::Candidate, Some("candidate-1".into())).unwrap();
        store.set_error("acme", "g1", Some("seed interrupted".into())).unwrap();
        let loaded = PhysicalStore::load(p.clone()).unwrap();
        assert!(loaded.owns_vm("candidate-1"));
        assert!(loaded.owns_vm("logical-vm-1"));
        assert!(loaded.reserves_database("acme"));
        let resumed = loaded.create(record("acme", "g1")).unwrap();
        assert_eq!(resumed.phase, PhysicalPhase::Candidate);
        assert_eq!(resumed.candidate_id.as_deref(), Some("candidate-1"));
        assert!(loaded.create(record("other", "g1")).is_err());
        let _ = std::fs::remove_file(p);
    }
    #[test]
    fn hostile_values_are_rejected() {
        let p = path("hostile"); let store = PhysicalStore::load(p.clone()).unwrap();
        let mut r = record("../acme", "g1"); assert!(store.create(r.clone()).is_err());
        r = record("acme", "../g1"); r.candidate_name = PhysicalRecord::candidate_name(&r.generation); assert!(store.create(r).is_err());
        let mut r = record("acme", "g1"); r.candidate_name = "pg-acme".into(); assert!(store.create(r).is_err()); let _ = std::fs::remove_file(p);
    }
    #[test]
    fn concurrent_duplicate_create_has_one_winner() {
        let p = path("concurrent"); let store = Arc::new(PhysicalStore::load(p.clone()).unwrap());
        std::thread::scope(|scope| { for generation in ["g1", "g2"] { let store = Arc::clone(&store); scope.spawn(move || { let _ = store.create(record("acme", generation)); }); } });
        assert_eq!(store.list().len(), 1); let _ = std::fs::remove_file(p);
    }
}
