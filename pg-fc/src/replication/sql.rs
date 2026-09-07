//! Every SQL statement and connection string replication needs, built as pure
//! functions.
//!
//! Kept apart from the code that executes them for one reason: this is the
//! part that is worth testing exhaustively and cannot be tested any other way.
//! A wrong `promote` ordering or an unquoted password only shows up against a
//! live two-node pairing, which no unit test can stand up — but the *text* of
//! the statement is checkable here, for free, on every build.

use std::net::Ipv4Addr;

use super::names;

/// A libpq keyword/value connection string, held as fields so the redacted
/// rendering is the only thing that can reach a log.
///
/// `hostaddr` is an [`Ipv4Addr`], not a string, and that is a type-level
/// statement of a real constraint: the guest microVMs ship with an empty
/// `/etc/resolv.conf`, so a hostname handed to a guest simply never resolves.
/// The pooler resolves peer hostnames on the *host* side, exactly as the S3
/// path pins IPs with `curl --resolve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conninfo {
    pub hostaddr: Ipv4Addr,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: String,
    /// `require` by default. Note this encrypts but does not authenticate the
    /// server: `verify-full` cannot work against a bare IP, and the guests
    /// carry no pinned CA.
    pub sslmode: String,
    /// Names this subscriber in the primary's `pg_stat_replication`.
    pub application_name: String,
}

impl Conninfo {
    /// The real thing. Only ever handed to a driver or to `PGPASSWORD`.
    pub fn to_libpq(&self) -> String {
        self.render(&self.password)
    }

    /// Identical, with the password replaced. Every error, log and journal
    /// path uses this; there is no code path that logs [`Self::to_libpq`].
    pub fn redacted(&self) -> String {
        self.render("***")
    }

    /// The same string with no `password` keyword at all, for a guest command
    /// that receives the password through `PGPASSWORD` in its environment
    /// instead — so the credential is in neither the planted script nor argv.
    pub fn without_password(&self) -> String {
        let mut out = self.render("");
        // Drop the whole keyword rather than leave an empty one, which libpq
        // would read as "no password supplied" only by accident.
        if let Some(i) = out.find(" password=") {
            let rest = &out[i + 1..];
            let end = rest.find(' ').map(|j| i + 1 + j).unwrap_or(out.len());
            out.replace_range(i..end, "");
        }
        out
    }

    fn render(&self, password: &str) -> String {
        format!(
            "hostaddr={} port={} dbname={} user={} password={} sslmode={} application_name={}",
            quote_conn(&self.hostaddr.to_string()),
            quote_conn(&self.port.to_string()),
            quote_conn(&self.dbname),
            quote_conn(&self.user),
            quote_conn(password),
            quote_conn(&self.sslmode),
            quote_conn(&self.application_name),
        )
    }
}

/// Quote a libpq keyword/value string value: wrap in single quotes and escape
/// backslashes and single quotes, per libpq's own rules.
///
/// Without this, a password containing a space would terminate the value and
/// the remainder would be parsed as further keywords — the connection-string
/// equivalent of SQL injection.
fn quote_conn(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('\'');
    for c in v.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Double every `"` and wrap: the same body as `vm::quote_ident`. Duplicated
/// rather than shared because these names never leave this module, and a
/// cross-module `pub(crate)` for three lines would be the larger coupling.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quote a SQL string literal (doubling embedded quotes). Valid under
/// `standard_conforming_strings`, which has been on by default since 9.1.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// --- primary side -----------------------------------------------------------

/// What the replication login needs to actually read the tables it streams.
/// `pg_read_all_data` is a predefined role (PG 14+); the schema `USAGE` grant
/// is what makes those tables reachable by name.
pub fn grant_repl_reads(role: &str) -> Vec<String> {
    vec![
        format!("GRANT pg_read_all_data TO {}", quote_ident(role)),
        format!("GRANT USAGE ON SCHEMA public TO {}", quote_ident(role)),
    ]
}

/// `FOR ALL TABLES` rather than an explicit table list, and that choice has a
/// consequence worth naming: it is the only thing that keeps a *growing*
/// schema replicating, because a table created later is published
/// automatically. It still has to exist on the subscriber and be picked up
/// with `ALTER SUBSCRIPTION ... REFRESH PUBLICATION` — logical replication
/// carries no DDL.
pub fn create_publication(pubname: &str) -> String {
    format!("CREATE PUBLICATION {} FOR ALL TABLES", quote_ident(pubname))
}

pub fn drop_publication(pubname: &str) -> String {
    format!("DROP PUBLICATION IF EXISTS {}", quote_ident(pubname))
}

/// Drop the slot **only when it is inactive**. Dropping a live one tears down
/// a subscriber mid-stream with no clean way for it to notice, and on a
/// teardown the subscriber has normally already let go — so if it hasn't,
/// that is a reason to stop, not to force it.
pub fn drop_slot_if_inactive(slot: &str) -> String {
    format!(
        "SELECT pg_drop_replication_slot(s.slot_name) FROM pg_replication_slots s \
         WHERE s.slot_name = {} AND NOT s.active",
        quote_literal(slot)
    )
}

/// Dropping a role needs its owned objects gone first, and `DROP OWNED BY` is
/// per-database — hence two statements against two different connections. The
/// caller runs `[0]` in the tenant database and `[1]` in `postgres`.
pub fn drop_repl_role(role: &str) -> [String; 2] {
    [
        format!("DROP OWNED BY {}", quote_ident(role)),
        format!("DROP ROLE IF EXISTS {}", quote_ident(role)),
    ]
}

/// Slot health and how far behind the subscriber is, joined to the live
/// sender when there is one. `$1` is the slot name.
pub const PRIMARY_STATUS_SQL: &str = "\
SELECT s.active,
       s.wal_status,
       s.confirmed_flush_lsn::text                                             AS confirmed_flush_lsn,
       pg_current_wal_lsn()::text                                              AS current_lsn,
       pg_wal_lsn_diff(pg_current_wal_lsn(), s.confirmed_flush_lsn)::int8       AS behind_bytes,
       r.state                                                                 AS sender_state,
       EXTRACT(EPOCH FROM r.write_lag)::float8                                 AS write_lag_s,
       EXTRACT(EPOCH FROM r.flush_lag)::float8                                 AS flush_lag_s,
       EXTRACT(EPOCH FROM r.replay_lag)::float8                                AS replay_lag_s
FROM pg_replication_slots s
LEFT JOIN pg_stat_replication r ON r.pid = s.active_pid
WHERE s.slot_name = $1";

/// Tables a publication covers that have no primary key and no explicit
/// `REPLICA IDENTITY`. `UPDATE`/`DELETE` on these errors at the publisher, so
/// this is a pre-flight warning — it cannot be auto-fixed without choosing a
/// replica identity on the tenant's behalf.
pub const NO_REPLICA_IDENTITY_SQL: &str = "\
SELECT n.nspname || '.' || c.relname
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'r'
  AND c.relreplident = 'd'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND NOT EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary)
ORDER BY 1";

// --- replica side -----------------------------------------------------------

/// `create_slot = true` means the *subscriber* creates the slot on the
/// primary. That ordering is deliberate and load-bearing: until this runs, the
/// primary owns nothing that pins WAL, so a setup that dies after the
/// publication is created leaves no disk hazard behind.
///
/// Cannot run inside a transaction block — issue it with `batch_execute` as a
/// lone statement, exactly as `vm::ensure_database` issues `CREATE DATABASE`.
pub fn create_subscription(
    sub: &str,
    conninfo: &Conninfo,
    pubname: &str,
    slot: &str,
    copy_data: bool,
    streaming: bool,
) -> String {
    format!(
        "CREATE SUBSCRIPTION {} CONNECTION {} PUBLICATION {} \
         WITH (slot_name = {}, create_slot = true, copy_data = {copy_data}, \
         streaming = {streaming}, enabled = true)",
        quote_ident(sub),
        quote_literal(&conninfo.to_libpq()),
        quote_ident(pubname),
        quote_literal(slot),
    )
}

pub fn refresh_subscription(sub: &str) -> String {
    format!(
        "ALTER SUBSCRIPTION {} REFRESH PUBLICATION",
        quote_ident(sub)
    )
}

/// The promote sequence, in order.
///
/// Returned as an ordered list rather than one batch specifically so the order
/// can be asserted in a test, because the middle statement is the whole trick:
/// `SET (slot_name = NONE)` **must** precede the drop. Without it
/// `DROP SUBSCRIPTION` tries to drop the slot on the primary and hangs or
/// fails when the primary is unreachable — which is exactly the failover case
/// this button exists for.
pub fn promote_statements(sub: &str) -> Vec<String> {
    let q = quote_ident(sub);
    vec![
        format!("ALTER SUBSCRIPTION {q} DISABLE"),
        format!("ALTER SUBSCRIPTION {q} SET (slot_name = NONE)"),
        format!("DROP SUBSCRIPTION {q}"),
    ]
}

/// Subscription health and initial-copy progress. `$1` is the subscription
/// name. `pg_stat_subscription_stats` is PG 15+, so the error columns are
/// selected separately by the caller when the server is new enough.
pub const REPLICA_STATUS_SQL: &str = "\
SELECT su.subenabled,
       st.pid IS NOT NULL                                                    AS worker_running,
       st.received_lsn::text                                                 AS received_lsn,
       st.latest_end_lsn::text                                               AS latest_end_lsn,
       EXTRACT(EPOCH FROM (now() - st.last_msg_receipt_time))::float8        AS last_msg_age_s,
       (SELECT count(*) FROM pg_subscription_rel r WHERE r.srsubid = su.oid)::int8 AS tables_total,
       (SELECT count(*) FROM pg_subscription_rel r
          WHERE r.srsubid = su.oid AND r.srsubstate = 'r')::int8             AS tables_ready
FROM pg_subscription su
LEFT JOIN pg_stat_subscription st ON st.subid = su.oid AND st.relid IS NULL
WHERE su.subname = $1";

/// Logical replication does not replicate sequence values, so every
/// serial/identity column on a replica is still sitting at its initial value
/// and the first insert after a promote collides. Re-seed each column-owned
/// sequence from the data that actually arrived.
///
/// Best-effort by nature: it is right for the overwhelmingly common
/// `serial`/`identity` shape and wrong for a sequence used by something other
/// than a table column, or a table emptied and refilled. Reported as a count,
/// never as a guarantee.
pub const FIX_SEQUENCES_SQL: &str = "\
DO $pgfc$
DECLARE r record; m bigint; n int := 0;
BEGIN
  FOR r IN
    SELECT s.oid::regclass::text AS seq, t.oid::regclass::text AS tbl, a.attname AS col
    FROM pg_class s
    JOIN pg_depend d  ON d.objid = s.oid AND d.classid = 'pg_class'::regclass
                     AND d.deptype IN ('a', 'i')
    JOIN pg_class t   ON t.oid = d.refobjid AND t.relkind = 'r'
    JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = d.refobjsubid
    WHERE s.relkind = 'S'
  LOOP
    EXECUTE format('SELECT COALESCE(max(%I), 0) FROM %s', r.col, r.tbl) INTO m;
    PERFORM setval(r.seq, m + 1, false);
    n := n + 1;
  END LOOP;
  RAISE NOTICE 'pgfc: reseeded % sequence(s)', n;
END
$pgfc$";

/// How many sequences [`FIX_SEQUENCES_SQL`] would touch, so the caller can
/// report a count without parsing a NOTICE.
pub const COUNT_SEQUENCES_SQL: &str = "\
SELECT count(*)::int8
FROM pg_class s
JOIN pg_depend d ON d.objid = s.oid AND d.classid = 'pg_class'::regclass
                AND d.deptype IN ('a', 'i')
JOIN pg_class t  ON t.oid = d.refobjid AND t.relkind = 'r'
WHERE s.relkind = 'S'";

/// Build the conninfo a replica uses to reach a primary, from the pieces the
/// two nodes exchange.
pub fn primary_conninfo(
    hostaddr: Ipv4Addr,
    port: u16,
    database: &str,
    sslmode: &str,
    local_node: &str,
) -> Conninfo {
    Conninfo {
        hostaddr,
        port,
        dbname: database.to_string(),
        user: names::repl_role_name(database),
        password: String::new(),
        sslmode: sslmode.to_string(),
        application_name: format!("pgfc_{local_node}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Conninfo {
        Conninfo {
            hostaddr: "203.0.113.10".parse().unwrap(),
            port: 6432,
            dbname: "acme".into(),
            user: "acme_pgfcrepl".into(),
            password: "p w'x\\y".into(),
            sslmode: "require".into(),
            application_name: "pgfc_node_b".into(),
        }
    }

    #[test]
    fn conninfo_quotes_a_password_with_a_space_quote_and_backslash() {
        let s = conn().to_libpq();
        // The password must survive as ONE value; if the space terminated it,
        // `x\y` would be parsed as a further keyword.
        assert!(s.contains(r"password='p w\'x\\y'"), "{s}");
        assert!(s.ends_with("application_name='pgfc_node_b'"), "{s}");
    }

    #[test]
    fn redacted_conninfo_never_contains_the_password() {
        let c = conn();
        let r = c.redacted();
        assert!(!r.contains("p w"), "{r}");
        assert!(r.contains("password='***'"), "{r}");
        // ...and still shows everything an operator needs to debug the link.
        assert!(
            r.contains("hostaddr='203.0.113.10'") && r.contains("dbname='acme'"),
            "{r}"
        );
    }

    #[test]
    fn without_password_drops_the_keyword_entirely() {
        let s = conn().without_password();
        assert!(!s.contains("password"), "{s}");
        assert!(
            s.contains("user='acme_pgfcrepl'") && s.contains("sslmode='require'"),
            "{s}"
        );
    }

    #[test]
    fn promote_detaches_the_slot_before_dropping_the_subscription() {
        let s = promote_statements("pgfc_sub_acme");
        assert_eq!(s.len(), 3);
        assert!(s[0].contains("DISABLE"), "{:?}", s);
        // The load-bearing order: without SET (slot_name = NONE) first, the
        // DROP reaches out to a primary that may be gone.
        let set_none = s
            .iter()
            .position(|x| x.contains("slot_name = NONE"))
            .unwrap();
        let drop = s
            .iter()
            .position(|x| x.starts_with("DROP SUBSCRIPTION"))
            .unwrap();
        assert!(set_none < drop, "{s:?}");
    }

    #[test]
    fn teardown_only_drops_an_inactive_slot() {
        let s = drop_slot_if_inactive("pgfc_acme_node_b");
        assert!(s.contains("NOT s.active"), "{s}");
        assert!(s.contains("'pgfc_acme_node_b'"), "{s}");
    }

    #[test]
    fn identifiers_and_literals_are_quoted() {
        // Defense in depth: these names are already validated upstream, but a
        // future caller must not be able to break out of one.
        let sub = create_subscription("s\"1", &conn(), "p\"1", "sl'ot", true, true);
        assert!(sub.contains("\"s\"\"1\""), "{sub}");
        assert!(sub.contains("\"p\"\"1\""), "{sub}");
        assert!(sub.contains("'sl''ot'"), "{sub}");
        // The whole conninfo is one literal, so its own quotes are doubled.
        assert!(sub.contains("create_slot = true"), "{sub}");
    }

    #[test]
    fn primary_conninfo_derives_the_replication_login() {
        let c = primary_conninfo(
            "10.0.0.1".parse().unwrap(),
            6432,
            "acme",
            "require",
            "node_b",
        );
        assert_eq!(c.user, "acme_pgfcrepl");
        assert_eq!(c.application_name, "pgfc_node_b");
        assert!(c.password.is_empty(), "the caller fills this in");
    }
}
