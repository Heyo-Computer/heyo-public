//! Positive-only name→id cache over the daemon's sandbox inventory.
//!
//! Exists to kill the O(fleet) cold start: resolving a schema the pooler has
//! never seen used to pull the ENTIRE `GET /deployed-sandboxes` listing to
//! find one VM by name (~1MB per resolve at fleet 5000, serialized behind a
//! slow daemon). With the cache, a repeat resolve is a map hit; a miss costs
//! one `?name=` daemon round-trip (see `vm::find_by_name_with_retry`), never
//! an inventory pull.
//!
//! Positive-only by design: heyvmd does NOT enforce name uniqueness, so a
//! cached miss must never be trusted — a cache miss ALWAYS goes to the
//! authoritative by-name daemon call before any create, and daemon-unreachable
//! fails the bring-up (as it always has), never creates. A stale *hit* is
//! also safe: `bring_up_existing` resolves by id, and its `Ok(None)`
//! ("deleted") path evicts the entry and falls through to the authoritative
//! lookup.
//!
//! Same idiom as `pending`: process-global `OnceLock`, `std::sync::Mutex`,
//! await-free sections. Purely in-memory — a restart starts cold and warms
//! from the first listing it absorbs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, String>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached sandbox id for `name`, if any. A hit is a hint (verify by id);
/// a miss means "ask the daemon", never "doesn't exist".
pub fn lookup(name: &str) -> Option<String> {
    cache().lock().unwrap().get(name).cloned()
}

/// Record `name` → `id` (bring-up succeeded, create was accepted, or the
/// daemon answered a by-name lookup).
pub fn insert(name: &str, id: &str) {
    cache()
        .lock()
        .unwrap()
        .insert(name.to_string(), id.to_string());
}

/// Drop every entry pointing at `id` — the VM was killed or found gone.
/// Retain-based because names are the keys and one id can (transiently)
/// appear under a rename.
pub fn remove_id(id: &str) {
    cache().lock().unwrap().retain(|_, v| v != id);
}

/// Merge a daemon listing into the cache (upsert per entry — never a
/// replace-all, so entries for VMs the listing didn't cover survive).
/// Every full listing the pooler pays for anywhere warms the cache for free.
pub fn absorb(infos: &[heyo_sdk::SandboxInfo]) {
    let mut cache = cache().lock().unwrap();
    for info in infos {
        if !info.name.is_empty() {
            cache.insert(info.name.clone(), info.id.clone());
        }
    }
}

/// Wipe the cache — test isolation only.
#[cfg(test)]
pub fn reset() {
    cache().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests mutate the one process-global cache; without this they race
    /// under cargo's parallel test threads and fail flakily.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn info(id: &str, name: &str) -> heyo_sdk::SandboxInfo {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "status": "running",
            "status_changed_at": "2026-08-19T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn insert_lookup_remove_roundtrip() {
        let _serial = SERIAL.lock().unwrap();
        reset();
        insert("pg-alpha", "sb-1");
        assert_eq!(lookup("pg-alpha").as_deref(), Some("sb-1"));
        assert_eq!(lookup("pg-beta"), None);
        remove_id("sb-1");
        assert_eq!(lookup("pg-alpha"), None);
    }

    #[test]
    fn absorb_merges_and_skips_nameless() {
        let _serial = SERIAL.lock().unwrap();
        reset();
        insert("pg-keep", "sb-keep");
        absorb(&[info("sb-2", "pg-two"), info("sb-3", "")]);
        // Upserted from the listing…
        assert_eq!(lookup("pg-two").as_deref(), Some("sb-2"));
        // …nameless entries (e.g. provisioning placeholders) are ignored…
        assert_eq!(lookup(""), None);
        // …and entries absent from the listing survive (merge, not replace).
        assert_eq!(lookup("pg-keep").as_deref(), Some("sb-keep"));
    }
}
