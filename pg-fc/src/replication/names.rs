//! Deriving the Postgres object names a replication pairing uses.
//!
//! Four names come out of one `(database, peer)` pair — a publication, a
//! subscription, a replication slot and a login role — and every one of them
//! is re-derived rather than stored on the hot paths that read them (status
//! sampling, teardown). That makes determinism load-bearing: a name that
//! changed between the call that created a slot and the call that drops it
//! would leave WAL pinned by an object nothing can address any more.
//!
//! The constraint that makes this non-trivial is Postgres' 63-byte identifier
//! limit. A database name may already be 63 bytes ([`crate::dedicated`]
//! enforces exactly that ceiling), so any prefix at all can overflow — and
//! Postgres *truncates* rather than erroring, which would silently collide two
//! pairings. [`fit`] handles that by truncating the body itself and appending
//! a hash of the full original, so long names stay distinct and stable.
//!
//! The output charset is `[a-z0-9_]`: that is the replication *slot* charset,
//! which is narrower than an identifier's, so one function can serve all four
//! names.

/// Postgres' identifier ceiling. An identifier longer than this is silently
/// truncated, not rejected.
pub const MAX_IDENT: usize = 63;

/// Bytes of hex appended when a name has to be shortened.
const HASH_LEN: usize = 8;

pub fn publication_name(database: &str) -> String {
    fit("pgfc_pub_", database)
}

pub fn subscription_name(database: &str) -> String {
    fit("pgfc_sub_", database)
}

/// Slot names carry the *peer* as well as the database: a primary may
/// eventually feed more than one replica, and two slots sharing a name is the
/// one collision Postgres will not catch for us.
pub fn slot_name(database: &str, peer: &str) -> String {
    fit("pgfc_", &format!("{database}_{peer}"))
}

/// The `REPLICATION` login a replica uses against this primary. Suffixed
/// rather than prefixed so it sorts next to its database in `\du`, and named
/// distinctly enough that it cannot be mistaken for the tenant's own role.
pub fn repl_role_name(database: &str) -> String {
    let body = format!("{database}_pgfcrepl");
    if body.len() <= MAX_IDENT {
        return body;
    }
    // Shorten the database half, keeping the suffix that identifies what this
    // role is — the part an operator reads.
    let keep = MAX_IDENT - "_pgfcrepl".len() - HASH_LEN - 1;
    format!(
        "{}_{}_pgfcrepl",
        &database[..keep.min(database.len())],
        hash8(database)
    )
}

/// Join `prefix` and `body` into an identifier that fits [`MAX_IDENT`],
/// truncating the body and appending a stable 8-hex digest of the *whole*
/// original body when it doesn't.
///
/// Deterministic by construction: same input, same output, forever. That is
/// the property teardown depends on.
fn fit(prefix: &str, body: &str) -> String {
    let body = sanitize(body);
    if prefix.len() + body.len() <= MAX_IDENT {
        return format!("{prefix}{body}");
    }
    // Room for the prefix, the digest and the underscore joining them.
    let keep = MAX_IDENT - prefix.len() - HASH_LEN - 1;
    format!("{prefix}{}_{}", &body[..keep], hash8(&body))
}

/// Map anything outside the slot charset onto `_`. Inputs are already
/// validated `[a-z][a-z0-9_]*` by [`crate::dedicated::validate_identifier`],
/// so this only ever fires on the `_` join in [`slot_name`] — it is here so a
/// future caller with a laxer input cannot produce an invalid slot name.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// FNV-1a, 64-bit, rendered as 8 hex characters. Not cryptographic and does
/// not need to be: it exists to keep two truncated names apart, and both are
/// operator-chosen rather than attacker-chosen.
fn hash8(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_slot_charset(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    #[test]
    fn short_names_are_the_obvious_ones() {
        assert_eq!(publication_name("acme"), "pgfc_pub_acme");
        assert_eq!(subscription_name("acme"), "pgfc_sub_acme");
        assert_eq!(slot_name("acme", "node_b"), "pgfc_acme_node_b");
        assert_eq!(repl_role_name("acme"), "acme_pgfcrepl");
    }

    #[test]
    fn names_fit_in_63_bytes_and_stay_stable() {
        // The ceiling `dedicated::validate_identifier` allows.
        let long = "a".repeat(63);
        for n in [
            publication_name(&long),
            subscription_name(&long),
            slot_name(&long, "node_b"),
            repl_role_name(&long),
        ] {
            assert!(n.len() <= MAX_IDENT, "{n} is {} bytes", n.len());
            assert!(valid_slot_charset(&n), "{n}");
        }
        // Stable across calls: teardown re-derives what setup created, so a
        // name that drifted would strand a slot pinning WAL.
        assert_eq!(publication_name(&long), publication_name(&long));
        assert_eq!(slot_name(&long, "b"), slot_name(&long, "b"));
    }

    #[test]
    fn truncated_names_stay_distinct() {
        let a = format!("{}x", "d".repeat(62));
        let b = format!("{}y", "d".repeat(62));
        assert_ne!(publication_name(&a), publication_name(&b));
        assert_ne!(repl_role_name(&a), repl_role_name(&b));
        // Including when only the peer half differs.
        let long = "e".repeat(60);
        assert_ne!(slot_name(&long, "node_b"), slot_name(&long, "node_c"));
    }

    #[test]
    fn slot_names_use_only_the_slot_charset() {
        // Slot names are narrower than identifiers; anything else is mapped.
        assert!(valid_slot_charset(&slot_name("acme", "node_b")));
        assert_eq!(slot_name("ac-me", "n"), "pgfc_ac_me_n");
        assert_eq!(slot_name("Acme", "n"), "pgfc_acme_n");
    }
}
