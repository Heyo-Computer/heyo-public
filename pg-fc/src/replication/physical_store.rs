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
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalFile<T> {
    version: u32,
    current: Vec<T>,
    history: Vec<T>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JournalDisk<T> {
    Journal(JournalFile<T>),
    Legacy(Vec<T>),
}

#[derive(Clone)]
struct Journal<T> {
    current: HashMap<String, T>,
    history: Vec<T>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalPhase {
    Intent,
    Creating,
    Candidate,
    Seeding,
    Verified,
    Prepared,
    Promoting,
    Promoted,
    Binding,
    Activated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoffGrant {
    pub candidate_id: String,
    pub peer: String,
    pub barrier_lsn: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalFence {
    pub phase: String,
    pub barrier_lsn: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalRecord {
    pub database: String,
    pub generation: String,
    /// The generation whose activation made this operation's source current.
    /// `None` is reserved for the initial logical/bootstrap operation.
    #[serde(default)]
    pub predecessor: Option<String>,
    pub candidate_name: String,
    #[serde(default)]
    pub repl: Option<super::wire::Login>,
    pub candidate_id: Option<String>,
    pub previous_vm_id: Option<String>,
    pub source_node: String,
    pub source_vm_id: String,
    pub system_identifier: String,
    pub pg_major: u32,
    pub slot: String,
    pub phase: PhysicalPhase,
    #[serde(default)]
    pub handoff_barrier: Option<String>,
    pub last_error: Option<String>,
}

impl PhysicalRecord {
    pub fn candidate_name(generation: &str) -> String {
        format!("repl-seed-{generation}")
    }

    pub fn handoff_started(&self) -> bool {
        phase_number(self.phase) >= phase_number(PhysicalPhase::Prepared)
    }
}

pub struct PhysicalStore {
    path: PathBuf,
    journal: Mutex<Journal<PhysicalRecord>>,
}

/// Source-side ownership of the physical slot.  This is intentionally not a
/// `PhysicalRecord`: a source is never a candidate and must not accidentally
/// participate in candidate activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalSourceRecord {
    pub database: String,
    pub generation: String,
    /// The incoming candidate generation which activated this source VM.
    /// `None` is reserved for the initial logical/bootstrap operation.
    #[serde(default)]
    pub predecessor: Option<String>,
    pub source_vm_id: String,
    #[serde(default)]
    pub repl: Option<super::wire::Login>,
    #[serde(default)]
    pub fence: Option<PhysicalFence>,
    pub system_identifier: String,
    pub pg_major: u32,
    pub slot: String,
    pub source_lsn: String,
    pub peer: String,
    #[serde(default)]
    pub handoff: Option<PhysicalHandoffGrant>,
    pub last_error: Option<String>,
}

pub struct PhysicalSourceStore {
    path: PathBuf,
    journal: Mutex<Journal<PhysicalSourceRecord>>,
}

impl PhysicalSourceStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let (current, history) = match std::fs::read(&path) {
            Ok(bytes) => decode_journal(&bytes, "physical source", &path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading physical source records at {}", path.display())),
        };
        let journal = source_journal(current, history)?;
        Ok(Self { path, journal: Mutex::new(journal) })
    }

    pub fn get(&self, database: &str) -> Option<PhysicalSourceRecord> {
        self.journal.lock().unwrap().current.get(database).cloned()
    }

    pub fn by_repl_role(&self, role: &str) -> Option<PhysicalSourceRecord> {
        self.journal.lock().unwrap().current.values().find(|r| r.repl.as_ref().is_some_and(|login| login.role == role)).cloned()
    }

    pub fn set_repl(&self, database: &str, generation: &str, login: super::wire::Login) -> Result<()> {
        validate_login(&login)?;
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical source")?;
        if current.generation != generation { bail!("stale physical source credential update"); }
        if let Some(existing) = &current.repl {
            if existing == &login { return Ok(()); }
            bail!("physical replication credential changed");
        }
        let mut next = journal.clone();
        next.current.get_mut(database).unwrap().repl = Some(login);
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(())
    }

    pub fn set_fence(&self, database: &str, generation: &str, phase: &str, barrier: Option<&str>) -> Result<()> {
        let fence = PhysicalFence { phase: phase.into(), barrier_lsn: barrier.map(str::to_owned) };
        validate_fence(&fence)?;
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical source")?;
        if current.generation != generation { bail!("stale physical source fence update"); }
        if let Some(grant) = &current.handoff {
            if phase != "ready" || barrier != Some(&grant.barrier_lsn) { bail!("irrevocable source barrier cannot change"); }
        }
        let mut next = journal.clone();
        next.current.get_mut(database).unwrap().fence = Some(fence);
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(())
    }

    pub fn clear_fence(&self, database: &str, generation: &str) -> Result<()> {
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical source")?;
        if current.generation != generation { bail!("stale physical source fence clear"); }
        if current.handoff.is_some() { bail!("irrevocable source fence cannot be cleared"); }
        if current.fence.as_ref().is_some_and(|f| f.phase != "unfencing") {
            bail!("invalidate the physical barrier before clearing its fence");
        }
        let mut next = journal.clone();
        next.current.get_mut(database).unwrap().fence = None;
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(())
    }

    pub fn owns_vm(&self, id: &str) -> bool {
        let journal = self.journal.lock().unwrap();
        journal.current.values().chain(journal.history.iter()).any(|r| r.source_vm_id == id)
    }

    /// Returns the permanent grant for this exact local source VM, if any.
    pub fn grant_for_source_vm(&self, database: &str, source_vm_id: &str) -> Option<PhysicalHandoffGrant> {
        let journal = self.journal.lock().unwrap();
        journal.current.values().chain(journal.history.iter())
            .find(|r| r.database == database && r.source_vm_id == source_vm_id)
            .and_then(|r| r.handoff.clone())
    }

    pub fn has_grant_for_source_vm(&self, database: &str, source_vm_id: &str) -> bool {
        self.grant_for_source_vm(database, source_vm_id).is_some()
    }

    pub fn create(&self, record: PhysicalSourceRecord) -> Result<PhysicalSourceRecord> {
        validate_source(&record)?;
        if record.predecessor.is_some() { bail!("bootstrap source cannot name a predecessor"); }
        let mut journal = self.journal.lock().unwrap();
        if let Some(existing) = journal.current.get(&record.database) {
            let mut observed = record.clone();
            observed.source_lsn = existing.source_lsn.clone();
            observed.last_error = existing.last_error.clone();
            if existing == &observed { return Ok(existing.clone()); }
            bail!("database {:?} already has a physical source operation", record.database);
        }
        ensure_source_unique(&journal, &record)?;
        let mut next = journal.clone();
        next.current.insert(record.database.clone(), record.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(record)
    }

    /// Atomically archives the old current source and installs a new source
    /// intent proven by an activated incoming candidate.
    pub fn create_successor(&self, record: PhysicalSourceRecord, incoming: &PhysicalRecord, current_binding: &str) -> Result<PhysicalSourceRecord> {
        validate_source(&record)?;
        validate_record(incoming)?;
        let candidate_id = incoming.candidate_id.as_deref().context("incoming candidate has no VM")?;
        if incoming.database != record.database || incoming.phase != PhysicalPhase::Activated
            || candidate_id != current_binding || record.source_vm_id != candidate_id
            || record.system_identifier != incoming.system_identifier || record.pg_major != incoming.pg_major
            || record.predecessor.as_deref() != Some(incoming.generation.as_str()) {
            bail!("successor source is not linked to the activated current binding");
        }
        if record.handoff.is_some() { bail!("new successor source cannot already contain a grant"); }
        let mut journal = self.journal.lock().unwrap();
        if let Some(existing) = journal.current.get(&record.database) {
            if existing.generation == record.generation {
                let mut intent = existing.clone();
                intent.source_lsn = record.source_lsn.clone(); intent.handoff = None; intent.last_error = None;
                if intent == record { return Ok(existing.clone()); }
                bail!("conflicting retry of successor source intent");
            }
            let grant = existing.handoff.as_ref().context("cannot supersede incomplete physical source operation")?;
            if incoming.predecessor.as_deref() != Some(existing.generation.as_str())
                || grant.peer != incoming.source_node || grant.candidate_id != incoming.source_vm_id {
                bail!("incoming activation does not prove the prior source grant");
            }
        }
        ensure_source_unique(&journal, &record)?;
        let mut next = journal.clone();
        if let Some(old) = next.current.insert(record.database.clone(), record.clone()) { next.history.push(old); }
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(record)
    }

    /// Irrevocably authorize one identity-bound promotion. Once persisted this
    /// record is never cleared; a retry must present the identical grant.
    pub fn grant_handoff(&self, database: &str, generation: &str, grant: PhysicalHandoffGrant) -> Result<PhysicalSourceRecord> {
        validate_token("candidate VM ID", &grant.candidate_id, 128)?;
        validate_token("peer", &grant.peer, 128)?;
        validate_lsn(&grant.barrier_lsn)?;
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical source operation")?;
        if current.generation != generation || current.peer != grant.peer { bail!("stale or mismatched physical handoff grant"); }
        if let Some(existing) = &current.handoff {
            if existing == &grant { return Ok(current.clone()); }
            bail!("a different physical handoff is already irrevocably authorized");
        }
        let mut updated = current.clone(); updated.handoff = Some(grant);
        let mut next = journal.clone(); next.current.insert(database.into(), updated.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(updated)
    }
}

impl PhysicalStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let (current, history) = match std::fs::read(&path) {
            Ok(bytes) => decode_journal(&bytes, "physical replication", &path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading physical replication records at {}", path.display())),
        };
        let journal = candidate_journal(current, history)?;
        Ok(Self { path, journal: Mutex::new(journal) })
    }

    pub fn list(&self) -> Vec<PhysicalRecord> {
        let mut records: Vec<_> = self.journal.lock().unwrap().current.values().cloned().collect();
        records.sort_by(|a, b| a.database.cmp(&b.database));
        records
    }

    pub fn get(&self, database: &str) -> Option<PhysicalRecord> {
        self.journal.lock().unwrap().current.get(database).cloned()
    }

    pub fn set_repl(&self, database: &str, generation: &str, login: super::wire::Login) -> Result<()> {
        validate_login(&login)?;
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical candidate")?;
        if current.generation != generation { bail!("stale physical candidate credential update"); }
        if let Some(existing) = &current.repl {
            if existing == &login { return Ok(()); }
            bail!("physical replication credential changed");
        }
        let mut next = journal.clone();
        next.current.get_mut(database).unwrap().repl = Some(login);
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(())
    }

    pub fn create(&self, record: PhysicalRecord) -> Result<PhysicalRecord> {
        validate_record(&record)?;
        if record.predecessor.is_some() { bail!("bootstrap candidate cannot name a predecessor"); }
        if record.phase != PhysicalPhase::Intent || record.candidate_id.is_some() {
            bail!("a new physical replication operation must start at intent without a candidate VM ID");
        }
        let mut journal = self.journal.lock().unwrap();
        if let Some(existing) = journal.current.get(&record.database) {
            let mut intent = existing.clone();
            intent.phase = PhysicalPhase::Intent;
            intent.candidate_id = None;
            intent.last_error = None;
            if intent == record { return Ok(existing.clone()); }
            bail!("database {:?} already has a physical replication operation", record.database);
        }
        ensure_candidate_unique(&journal, &record)?;
        let mut next = journal.clone();
        next.current.insert(record.database.clone(), record.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(record)
    }

    /// Atomically archives an activated candidate and installs the next intent,
    /// using a local source grant as ancestry and binding evidence.
    pub fn create_successor(&self, record: PhysicalRecord, preceding: &PhysicalSourceRecord, current_binding: &str) -> Result<PhysicalRecord> {
        validate_record(&record)?;
        validate_source(preceding)?;
        let grant = preceding.handoff.as_ref().context("preceding source has no handoff grant")?;
        if record.phase != PhysicalPhase::Intent || record.candidate_id.is_some() || record.handoff_barrier.is_some()
            || record.database != preceding.database || record.predecessor.as_deref() != Some(preceding.generation.as_str())
            || record.source_node != grant.peer || record.source_vm_id != grant.candidate_id
            || record.system_identifier != preceding.system_identifier || record.pg_major != preceding.pg_major
            || record.previous_vm_id.as_deref() != Some(current_binding) || preceding.source_vm_id != current_binding {
            bail!("successor candidate is not linked to the preceding local source grant and binding");
        }
        let mut journal = self.journal.lock().unwrap();
        if let Some(existing) = journal.current.get(&record.database) {
            if existing.generation == record.generation {
                let mut intent = existing.clone();
                intent.phase = PhysicalPhase::Intent; intent.candidate_id = None; intent.handoff_barrier = None; intent.last_error = None;
                if intent == record { return Ok(existing.clone()); }
                bail!("conflicting retry of successor candidate intent");
            }
            if existing.phase != PhysicalPhase::Activated || existing.candidate_id.as_deref() != Some(current_binding)
                || preceding.predecessor.as_deref() != Some(existing.generation.as_str()) {
                bail!("old candidate is not the activation replaced by the preceding source");
            }
        } else if preceding.predecessor.is_some() {
            bail!("first local successor candidate requires a bootstrap source");
        }
        ensure_candidate_unique(&journal, &record)?;
        let mut next = journal.clone();
        if let Some(old) = next.current.insert(record.database.clone(), record.clone()) { next.history.push(old); }
        persist_journal(&self.path, &next)?;
        *journal = next;
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
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).with_context(|| format!("no physical replication operation for database {database:?}"))?;
        if current.generation != generation || current.phase != expected_phase {
            bail!("stale physical replication update for database {database:?}: generation or phase no longer matches");
        }
        if phase_number(next_phase) != phase_number(expected_phase) + 1 {
            bail!("illegal physical replication phase transition from {expected_phase:?} to {next_phase:?}");
        }
        let mut updated = current.clone();
        if let Some(id) = candidate_id {
            validate_token("candidate VM ID", &id, 128)?;
            if local_candidate_vm_owned(&journal, &id) && current.candidate_id.as_deref() != Some(&id) {
                bail!("candidate VM ID is already owned by physical history");
            }
            if let Some(bound) = &updated.candidate_id {
                if bound != &id { bail!("candidate VM ID is already bound and cannot be changed"); }
            } else {
                updated.candidate_id = Some(id);
            }
        }
        updated.phase = next_phase;
        updated.last_error = None;
        validate_record(&updated)?;
        let mut next = journal.clone();
        next.current.insert(database.to_string(), updated.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(updated)
    }

    /// Persist the peer-verified source grant before any guest promotion effect.
    pub fn begin_handoff(&self, database: &str, generation: &str, barrier: &str) -> Result<PhysicalRecord> {
        validate_lsn(barrier)?;
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).context("no physical candidate")?;
        if current.generation != generation { bail!("stale physical handoff generation"); }
        if current.handoff_started() {
            if current.handoff_barrier.as_deref() == Some(barrier) { return Ok(current.clone()); }
            bail!("physical handoff barrier changed");
        }
        if current.phase != PhysicalPhase::Verified { bail!("physical candidate is not verified"); }
        let mut updated = current.clone();
        updated.handoff_barrier = Some(barrier.to_owned());
        updated.phase = PhysicalPhase::Prepared;
        validate_record(&updated)?;
        let mut next = journal.clone();
        next.current.insert(database.to_owned(), updated.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(updated)
    }

    pub fn set_error(&self, database: &str, generation: &str, error: Option<String>) -> Result<PhysicalRecord> {
        let mut journal = self.journal.lock().unwrap();
        let current = journal.current.get(database).with_context(|| format!("no physical replication operation for database {database:?}"))?;
        if current.generation != generation { bail!("stale physical replication error update for database {database:?}"); }
        if let Some(message) = &error { validate_error(message)?; }
        let mut updated = current.clone();
        updated.last_error = error;
        let mut next = journal.clone();
        next.current.insert(database.to_string(), updated.clone());
        persist_journal(&self.path, &next)?;
        *journal = next;
        Ok(updated)
    }

    pub fn owns_vm(&self, id: &str) -> bool {
        let journal = self.journal.lock().unwrap();
        journal.current.values().chain(journal.history.iter()).any(|r| {
            r.candidate_id.as_deref() == Some(id) || r.previous_vm_id.as_deref() == Some(id)
        })
    }

    pub fn reserves_database(&self, database: &str) -> bool {
        self.journal.lock().unwrap().current.contains_key(database)
    }
}

fn phase_number(phase: PhysicalPhase) -> u8 {
    match phase {
        PhysicalPhase::Intent => 0,
        PhysicalPhase::Creating => 1,
        PhysicalPhase::Candidate => 2,
        PhysicalPhase::Seeding => 3,
        PhysicalPhase::Verified => 4,
        PhysicalPhase::Prepared => 5,
        PhysicalPhase::Promoting => 6,
        PhysicalPhase::Promoted => 7,
        PhysicalPhase::Binding => 8,
        PhysicalPhase::Activated => 9,
    }
}

fn validate_record(record: &PhysicalRecord) -> Result<()> {
    validate_pg_identifier("database", &record.database)?;
    validate_generation(&record.generation)?;
    if let Some(login) = &record.repl { validate_login(login)?; }
    if let Some(predecessor) = &record.predecessor {
        validate_generation(predecessor)?;
        if predecessor == &record.generation { bail!("physical operation cannot be its own predecessor"); }
    }
    if record.handoff_started() {
        validate_lsn(record.handoff_barrier.as_deref().context("handoff phase requires durable source barrier")?)?;
    } else if record.handoff_barrier.is_some() {
        bail!("preparation cannot contain a handoff barrier");
    }
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
    if let Some(login) = &record.repl { validate_login(login)?; }
    if let Some(fence) = &record.fence { validate_fence(fence)?; }
    if let Some(predecessor) = &record.predecessor {
        validate_generation(predecessor)?;
        if predecessor == &record.generation { bail!("physical source cannot be its own predecessor"); }
    }
    validate_token("source VM ID", &record.source_vm_id, 128)?;
    validate_token("peer", &record.peer, 128)?;
    validate_pg_identifier("replication slot", &record.slot)?;
    if record.system_identifier.is_empty() || record.system_identifier.len() > 20 || !record.system_identifier.bytes().all(|b| b.is_ascii_digit()) { bail!("PostgreSQL system identifier must be a numeric string"); }
    if record.pg_major == 0 || record.source_lsn.is_empty() || record.source_lsn.len() > 32 { bail!("invalid physical source identity"); }
    if let Some(grant) = &record.handoff {
        validate_token("candidate VM ID", &grant.candidate_id, 128)?;
        validate_token("peer", &grant.peer, 128)?;
        validate_lsn(&grant.barrier_lsn)?;
        if grant.peer != record.peer { bail!("handoff peer differs from source owner"); }
    }
    if let Some(error) = &record.last_error { validate_error(error)?; }
    Ok(())
}

fn validate_login(login: &super::wire::Login) -> Result<()> {
    validate_pg_identifier("replication role", &login.role)?;
    if login.password.is_empty() || login.password.contains('\0') { bail!("invalid replication credential"); }
    Ok(())
}

fn validate_fence(fence: &PhysicalFence) -> Result<()> {
    if !matches!(fence.phase.as_str(), "intent" | "closing_admission" | "draining_startups" | "draining" | "barrier" | "ready" | "unfencing") {
        bail!("invalid physical source fence phase");
    }
    if fence.phase == "ready" {
        validate_lsn(fence.barrier_lsn.as_deref().context("ready physical fence lacks a barrier")?)?;
    } else if fence.barrier_lsn.is_some() { bail!("incomplete physical fence cannot have a barrier"); }
    Ok(())
}

fn validate_lsn(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty() || part.len() > 8
        || !part.bytes().all(|b| b.is_ascii_hexdigit())) {
        bail!("invalid WAL barrier LSN");
    }
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

fn decode_journal<T: DeserializeOwned>(bytes: &[u8], label: &str, path: &Path) -> Result<(Vec<T>, Vec<T>)> {
    match serde_json::from_slice(bytes).with_context(|| format!("parsing {label} records at {}", path.display()))? {
        JournalDisk::Legacy(current) => Ok((current, Vec::new())),
        JournalDisk::Journal(file) if file.version == 1 => Ok((file.current, file.history)),
        JournalDisk::Journal(file) => bail!("unsupported physical journal version {}", file.version),
    }
}

fn source_journal(current: Vec<PhysicalSourceRecord>, history: Vec<PhysicalSourceRecord>) -> Result<Journal<PhysicalSourceRecord>> {
    let mut by_database = HashMap::new();
    let mut generations = std::collections::HashSet::new();
    let mut source_vms = std::collections::HashSet::new();
    for record in current.iter().chain(history.iter()) {
        validate_source(record)?;
        if !generations.insert(record.generation.clone()) || !source_vms.insert(record.source_vm_id.clone()) {
            bail!("duplicate physical source generation or VM identity");
        }
    }
    for record in &history {
        if record.handoff.is_none() { bail!("historical physical source operation lacks an irrevocable grant"); }
    }
    for record in current {
        if by_database.insert(record.database.clone(), record).is_some() { bail!("duplicate current physical source record for a database"); }
    }
    if history.iter().any(|r| !by_database.contains_key(&r.database)) { bail!("physical source history has no current operation"); }
    Ok(Journal { current: by_database, history })
}

fn candidate_journal(current: Vec<PhysicalRecord>, history: Vec<PhysicalRecord>) -> Result<Journal<PhysicalRecord>> {
    let mut by_database = HashMap::new();
    let mut generations = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    let mut candidate_ids = std::collections::HashSet::new();
    for record in current.iter().chain(history.iter()) {
        validate_record(record).with_context(|| format!("invalid physical replication record for {:?}", record.database))?;
        if !generations.insert(record.generation.clone()) || !names.insert(record.candidate_name.clone())
            || record.candidate_id.as_ref().is_some_and(|id| !candidate_ids.insert(id.clone())) {
            bail!("duplicate physical candidate generation, name, or VM identity");
        }
    }
    for record in &history {
        if record.phase != PhysicalPhase::Activated { bail!("historical physical candidate was not activated"); }
    }
    for record in current {
        if by_database.insert(record.database.clone(), record).is_some() { bail!("duplicate current physical replication record for a database"); }
    }
    if history.iter().any(|r| !by_database.contains_key(&r.database)) { bail!("physical candidate history has no current operation"); }
    Ok(Journal { current: by_database, history })
}

fn ensure_source_unique(journal: &Journal<PhysicalSourceRecord>, record: &PhysicalSourceRecord) -> Result<()> {
    if journal.current.values().chain(journal.history.iter()).any(|r| r.generation == record.generation || r.source_vm_id == record.source_vm_id) {
        bail!("physical source generation or VM identity is already permanently owned");
    }
    Ok(())
}

fn ensure_candidate_unique(journal: &Journal<PhysicalRecord>, record: &PhysicalRecord) -> Result<()> {
    if journal.current.values().chain(journal.history.iter()).any(|r| r.generation == record.generation || r.candidate_name == record.candidate_name) {
        bail!("physical candidate generation or name is already permanently owned");
    }
    Ok(())
}

fn local_candidate_vm_owned(journal: &Journal<PhysicalRecord>, id: &str) -> bool {
    journal.current.values().chain(journal.history.iter()).any(|r| {
        r.candidate_id.as_deref() == Some(id) || r.previous_vm_id.as_deref() == Some(id)
    })
}

/// After rename, a failed directory sync leaves durability ambiguous. Stop the
/// controller rather than serve requests from memory that disagrees with disk;
/// restart reloads the journal before admission or handoff can resume.
fn persist_journal<T: Clone + Serialize>(path: &Path, journal: &Journal<T>) -> Result<()> {
    let file = JournalFile { version: 1, current: journal.current.values().cloned().collect(), history: journal.history.clone() };
    let bytes = serde_json::to_vec_pretty(&file).context("serializing physical replication journal")?;
    let tmp = path.with_extension("physical.json.tmp");
    let result = (|| -> Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        let directory = std::fs::File::open(parent).context("opening physical replication records directory")?;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
            .with_context(|| format!("opening temporary physical replication file beside {}", path.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming physical replication records into {}", path.display()))?;
        if let Err(error) = directory.sync_all() {
            tracing::error!(%error, "physical journal durability is ambiguous; stopping controller to prevent stale-memory admission");
            std::process::exit(1);
        }
        Ok(())
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
        PhysicalRecord { database: database.into(), generation: generation.into(), predecessor: None, candidate_name: PhysicalRecord::candidate_name(generation), repl: None, candidate_id: None, previous_vm_id: Some("logical-vm-1".into()), source_node: "eu2".into(), source_vm_id: "source-vm-1".into(), system_identifier: "7431234567890123456".into(), pg_major: 18, slot: "physical_acme".into(), phase: PhysicalPhase::Intent, handoff_barrier: None, last_error: None }
    }
    fn source(database: &str, generation: &str) -> PhysicalSourceRecord {
        PhysicalSourceRecord { database: database.into(), generation: generation.into(), predecessor: None, source_vm_id: "source-vm-1".into(),
            repl: None, fence: None,
            system_identifier: "7431234567890123456".into(), pg_major: 18, slot: "physical_acme".into(),
            source_lsn: "0/16B6C50".into(), peer: "eu1".into(), handoff: None, last_error: None }
    }

    #[test]
    fn physical_unfence_invalidates_barrier_and_cannot_undo_grant() {
        let p = path("physical-unfence");
        let store = PhysicalSourceStore::load(p.clone()).unwrap();
        store.create(source("acme", "g1")).unwrap();
        store.set_fence("acme", "g1", "ready", Some("0/123")).unwrap();
        assert!(store.clear_fence("acme", "g1").is_err());
        assert!(store.set_fence("acme", "stale", "unfencing", None).is_err());
        store.set_fence("acme", "g1", "unfencing", None).unwrap();
        let store = PhysicalSourceStore::load(p.clone()).unwrap();
        assert_eq!(store.get("acme").unwrap().fence.unwrap().barrier_lsn, None);
        store.clear_fence("acme", "g1").unwrap();
        assert!(PhysicalSourceStore::load(p.clone()).unwrap().get("acme").unwrap().fence.is_none());
        store.set_fence("acme", "g1", "ready", Some("0/456")).unwrap();
        store.grant_handoff("acme", "g1", PhysicalHandoffGrant {
            candidate_id: "candidate-1".into(), peer: "eu1".into(), barrier_lsn: "0/456".into(),
        }).unwrap();
        assert!(store.set_fence("acme", "g1", "unfencing", None).is_err());
        assert!(store.clear_fence("acme", "g1").is_err());
        assert_eq!(store.get("acme").unwrap().fence.unwrap().barrier_lsn.as_deref(), Some("0/456"));
        std::fs::remove_file(p).unwrap();
    }

    fn activate(store: &PhysicalStore, generation: &str, candidate: &str, barrier: &str) -> PhysicalRecord {
        store.advance("acme", generation, PhysicalPhase::Intent, PhysicalPhase::Creating, None).unwrap();
        store.advance("acme", generation, PhysicalPhase::Creating, PhysicalPhase::Candidate, Some(candidate.into())).unwrap();
        store.advance("acme", generation, PhysicalPhase::Candidate, PhysicalPhase::Seeding, None).unwrap();
        store.advance("acme", generation, PhysicalPhase::Seeding, PhysicalPhase::Verified, None).unwrap();
        store.begin_handoff("acme", generation, barrier).unwrap();
        for (from, to) in [(PhysicalPhase::Prepared, PhysicalPhase::Promoting),
            (PhysicalPhase::Promoting, PhysicalPhase::Promoted), (PhysicalPhase::Promoted, PhysicalPhase::Binding),
            (PhysicalPhase::Binding, PhysicalPhase::Activated)] {
            store.advance("acme", generation, from, to, None).unwrap();
        }
        store.get("acme").unwrap()
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
    #[test]
    fn handoff_grant_is_irrevocable_idempotent_and_restart_safe() {
        let p = path("grant"); let store = PhysicalSourceStore::load(p.clone()).unwrap();
        store.create(source("acme", "g1")).unwrap();
        let grant = PhysicalHandoffGrant { candidate_id: "candidate-1".into(), peer: "eu1".into(), barrier_lsn: "0/16B6C50".into() };
        store.grant_handoff("acme", "g1", grant.clone()).unwrap();
        store.grant_handoff("acme", "g1", grant.clone()).unwrap();
        assert!(store.grant_handoff("acme", "g1", PhysicalHandoffGrant { candidate_id: "candidate-2".into(), ..grant.clone() }).is_err());
        assert_eq!(PhysicalSourceStore::load(p.clone()).unwrap().get("acme").unwrap().handoff, Some(grant));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn candidate_handoff_requires_a_durable_fixed_barrier_at_every_phase() {
        let p = path("candidate-handoff");
        let store = PhysicalStore::load(p.clone()).unwrap();
        store.create(record("acme", "g1")).unwrap();
        assert!(store.begin_handoff("acme", "g1", "0/ABC").is_err());
        store.advance("acme", "g1", PhysicalPhase::Intent, PhysicalPhase::Creating, None).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Creating, PhysicalPhase::Candidate, Some("candidate-1".into())).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Candidate, PhysicalPhase::Seeding, None).unwrap();
        store.advance("acme", "g1", PhysicalPhase::Seeding, PhysicalPhase::Verified, None).unwrap();
        assert!(!store.get("acme").unwrap().handoff_started());
        assert!(store.advance("acme", "g1", PhysicalPhase::Verified, PhysicalPhase::Prepared, None).is_err());
        for bad in ["/", "1/", "/A", "1/100000000", "G/1"] {
            assert!(store.begin_handoff("acme", "g1", bad).is_err());
        }
        assert!(store.begin_handoff("acme", "stale", "0/ABC").is_err());
        store.begin_handoff("acme", "g1", "0/ABC").unwrap();
        for (from, to) in [(PhysicalPhase::Prepared, PhysicalPhase::Promoting),
            (PhysicalPhase::Promoting, PhysicalPhase::Promoted), (PhysicalPhase::Promoted, PhysicalPhase::Binding),
            (PhysicalPhase::Binding, PhysicalPhase::Activated)] {
            let restarted = PhysicalStore::load(p.clone()).unwrap();
            assert_eq!(restarted.begin_handoff("acme", "g1", "0/ABC").unwrap().phase, from);
            assert!(restarted.begin_handoff("acme", "g1", "0/ABD").is_err());
            restarted.advance("acme", "g1", from, to, None).unwrap();
        }
        let mut corrupt = PhysicalStore::load(p.clone()).unwrap().get("acme").unwrap();
        corrupt.handoff_barrier = None;
        std::fs::write(&p, serde_json::to_vec(&vec![corrupt]).unwrap()).unwrap();
        assert!(PhysicalStore::load(p.clone()).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn failed_grant_persistence_keeps_journal_open() {
        let dir = path("grant-fail"); std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("source.json"); let store = PhysicalSourceStore::load(p.clone()).unwrap();
        store.create(source("acme", "g1")).unwrap();
        std::fs::remove_file(&p).unwrap(); std::fs::remove_dir(&dir).unwrap();
        let grant = PhysicalHandoffGrant { candidate_id: "candidate-1".into(), peer: "eu1".into(), barrier_lsn: "0/16B6C50".into() };
        assert!(store.grant_handoff("acme", "g1", grant).is_err());
        assert!(store.get("acme").unwrap().handoff.is_none());
    }

    #[test]
    fn bounded_journals_support_three_identity_bound_handoffs() {
        let us_source_path = path("three-us-source"); let us_candidate_path = path("three-us-candidate");
        let eu_source_path = path("three-eu-source"); let eu_candidate_path = path("three-eu-candidate");

        // Deployed legacy arrays: us3 U0 -> eu1 E1, replacing logical E0.
        let mut g1s = source("acme", "g1"); g1s.source_vm_id = "u0".into(); g1s.peer = "eu1".into();
        let mut g1c = record("acme", "g1"); g1c.previous_vm_id = Some("e0".into()); g1c.source_node = "us3".into(); g1c.source_vm_id = "u0".into();
        std::fs::write(&us_source_path, serde_json::to_vec(&vec![g1s]).unwrap()).unwrap();
        std::fs::write(&eu_candidate_path, serde_json::to_vec(&vec![g1c]).unwrap()).unwrap();
        let us_source = PhysicalSourceStore::load(us_source_path.clone()).unwrap();
        let eu_candidate = PhysicalStore::load(eu_candidate_path.clone()).unwrap();
        us_source.grant_handoff("acme", "g1", PhysicalHandoffGrant { candidate_id: "e1".into(), peer: "eu1".into(), barrier_lsn: "0/101".into() }).unwrap();
        let g1_activation = activate(&eu_candidate, "g1", "e1", "0/101");
        assert!(eu_candidate.advance("acme", "g0", PhysicalPhase::Intent, PhysicalPhase::Creating, None).is_err());

        // eu1 E1 -> us3 U1. eu1 has no prior source; us3 has no prior candidate.
        let eu_source = PhysicalSourceStore::load(eu_source_path.clone()).unwrap();
        let mut g2s = source("acme", "g2"); g2s.predecessor = Some("g1".into()); g2s.source_vm_id = "e1".into(); g2s.peer = "us3".into();
        eu_source.create_successor(g2s, &g1_activation, "e1").unwrap();
        eu_source.grant_handoff("acme", "g2", PhysicalHandoffGrant { candidate_id: "u1".into(), peer: "us3".into(), barrier_lsn: "0/202".into() }).unwrap();
        let us_candidate = PhysicalStore::load(us_candidate_path.clone()).unwrap();
        let mut g2c = record("acme", "g2"); g2c.predecessor = Some("g1".into()); g2c.previous_vm_id = Some("u0".into()); g2c.source_node = "eu1".into(); g2c.source_vm_id = "e1".into();
        us_candidate.create_successor(g2c.clone(), &us_source.get("acme").unwrap(), "u0").unwrap();
        assert_eq!(us_candidate.create_successor(g2c, &us_source.get("acme").unwrap(), "u0").unwrap().phase, PhysicalPhase::Intent);
        let g2_activation = activate(&us_candidate, "g2", "u1", "0/202");

        // us3 U1 -> eu1 E2. Both old current operations are archived atomically.
        let us_source = PhysicalSourceStore::load(us_source_path.clone()).unwrap();
        let mut g3s = source("acme", "g3"); g3s.predecessor = Some("g2".into()); g3s.source_vm_id = "u1".into(); g3s.peer = "eu1".into();
        assert!(us_source.create_successor(g3s.clone(), &g2_activation, "stale-u0").is_err());
        us_source.create_successor(g3s, &g2_activation, "u1").unwrap();
        us_source.grant_handoff("acme", "g3", PhysicalHandoffGrant { candidate_id: "e2".into(), peer: "eu1".into(), barrier_lsn: "0/303".into() }).unwrap();
        let mut g3_retry = source("acme", "g3"); g3_retry.predecessor = Some("g2".into()); g3_retry.source_vm_id = "u1".into(); g3_retry.peer = "eu1".into();
        assert!(us_source.create_successor(g3_retry, &g2_activation, "u1").unwrap().handoff.is_some());
        let eu_candidate = PhysicalStore::load(eu_candidate_path.clone()).unwrap();
        let mut g3c = record("acme", "g3"); g3c.predecessor = Some("g2".into()); g3c.previous_vm_id = Some("e1".into()); g3c.source_node = "us3".into(); g3c.source_vm_id = "u1".into();
        let g2_source = PhysicalSourceStore::load(eu_source_path.clone()).unwrap().get("acme").unwrap();
        let mut wrong = g3c.clone(); wrong.predecessor = Some("g1".into());
        assert!(eu_candidate.create_successor(wrong, &g2_source, "e1").is_err());
        eu_candidate.create_successor(g3c, &g2_source, "e1").unwrap();
        assert!(eu_candidate.advance("acme", "g1", PhysicalPhase::Activated, PhysicalPhase::Intent, None).is_err());
        assert!(eu_candidate.advance("acme", "g3", PhysicalPhase::Intent, PhysicalPhase::Creating, Some("e1".into())).is_err());

        let us_source = PhysicalSourceStore::load(us_source_path.clone()).unwrap();
        assert!(us_source.has_grant_for_source_vm("acme", "u0"));
        assert!(us_source.has_grant_for_source_vm("acme", "u1"));
        assert!(!us_source.has_grant_for_source_vm("acme", "e2"));
        assert!(us_source.owns_vm("u0") && us_source.owns_vm("u1"));
        assert!(eu_candidate.owns_vm("e0") && eu_candidate.owns_vm("e1"));
        for p in [us_source_path, us_candidate_path, eu_source_path, eu_candidate_path] { let _ = std::fs::remove_file(p); }
    }

    #[test]
    fn failed_successor_persistence_preserves_current_ownership() {
        let dir = path("successor-fail"); std::fs::create_dir_all(&dir).unwrap(); let p = dir.join("source.json");
        let store = PhysicalSourceStore::load(p.clone()).unwrap();
        let mut old = source("acme", "g1"); old.source_vm_id = "u0".into(); old.peer = "eu1".into(); store.create(old).unwrap();
        store.grant_handoff("acme", "g1", PhysicalHandoffGrant { candidate_id: "e1".into(), peer: "eu1".into(), barrier_lsn: "0/1".into() }).unwrap();
        let mut incoming = record("acme", "g2"); incoming.predecessor = Some("g1".into()); incoming.source_node = "eu1".into(); incoming.source_vm_id = "e1".into(); incoming.candidate_id = Some("u1".into()); incoming.phase = PhysicalPhase::Activated; incoming.handoff_barrier = Some("0/2".into());
        let mut next = source("acme", "g3"); next.predecessor = Some("g2".into()); next.source_vm_id = "u1".into();
        std::fs::remove_file(&p).unwrap(); std::fs::remove_dir(&dir).unwrap();
        assert!(store.create_successor(next, &incoming, "u1").is_err());
        assert_eq!(store.get("acme").unwrap().generation, "g1");
        assert!(store.owns_vm("u0") && !store.owns_vm("u1"));
    }
}
