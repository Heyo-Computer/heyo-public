//! The JSON contract two pg-fc nodes speak to each other.
//!
//! Both nodes run the same binary, so this is one set of types rather than a
//! client half and a server half. It is also the one part of replication that
//! is a *versioned interface*: a node may be talking to a peer running an
//! older or newer build, and the failure mode of a silent mismatch here is a
//! pairing that half-exists on two hosts. Hence the round-trip test, and hence
//! `#[serde(default)]` on everything that was not in the first version.
//!
//! One request carries secrets — [`ProvisionReplica`], which hands the replica
//! both the tenant credential (so the same connection string works after a
//! promote) and the replication login. It must only ever travel over the
//! peer's authenticated admin API, and `Debug` is implemented by hand so a
//! stray log line cannot print either password.

use serde::{Deserialize, Serialize};

/// `GET /api/replication/peer/node` — the handshake before anything is
/// created, so a mismatch is a clean refusal rather than half a pairing.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct NodeInfo {
    /// The peer's `PG_VM_POOL_NODE_NAME`. Compared against this node's own to
    /// refuse self-peering, which would otherwise produce a database
    /// subscribing to itself.
    pub node: String,
    /// Whether the peer has `PG_VM_POOL_REPLICATION` enabled at all.
    pub replication_enabled: bool,
    /// `server_version_num` of a VM the peer can reach, when it knows one.
    /// Used to refuse a publisher newer than its subscriber.
    #[serde(default)]
    pub server_version_num: Option<i32>,
    /// Whether the peer terminates TLS on its pooler listener. A primary
    /// refuses to hand out a credential to a replica that would send it in
    /// cleartext.
    #[serde(default)]
    pub tls: bool,
    /// The peer's pooler port, so an operator can sanity-check the peer record
    /// they typed against what the peer actually believes.
    #[serde(default)]
    pub pg_listen_port: Option<u16>,
}

/// One end of the replication link, as the *other* node must dial it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrimaryEndpoint {
    /// An IPv4 literal, never a hostname: this is handed to a guest, and the
    /// microVMs ship with an empty `/etc/resolv.conf`. The primary resolves it
    /// host-side before sending.
    pub hostaddr: String,
    pub port: u16,
    pub sslmode: String,
}

impl std::fmt::Debug for PrimaryEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} ({})", self.hostaddr, self.port, self.sslmode)
    }
}

/// A credential handed across the link.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Login {
    pub role: String,
    pub password: String,
}

/// Hand-written so neither password can reach a log through a `{:?}`.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Login {{ role: {:?}, password: *** }}", self.role)
    }
}

/// `POST /api/replication/peer/replicas` — the primary asking a peer to build
/// the replica. The only request that carries secrets.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ProvisionReplica {
    pub database: String,
    /// The *primary's* node name, which becomes the replica's `peer`.
    pub peer: String,
    /// The tenant's own credential, mirrored onto the replica so the same
    /// connection string works against either node after a promote. This is
    /// what makes failover a DNS change rather than a re-credentialing.
    pub tenant: Login,
    /// The `REPLICATION` login the replica authenticates to the primary with.
    pub repl: Login,
    pub primary: PrimaryEndpoint,
    pub publication: String,
    pub subscription: String,
    pub slot: String,
    #[serde(default = "yes")]
    pub copy_data: bool,
    #[serde(default = "yes")]
    pub streaming: bool,
    /// The primary's `server_version_num`, so the replica can refuse a
    /// publisher newer than itself rather than fail obscurely at apply time.
    #[serde(default)]
    pub primary_server_version_num: Option<i32>,
}

fn yes() -> bool {
    true
}

/// A replication record as the API renders it. Carries no password — the only
/// place a password crosses is [`ProvisionReplica`].
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct RecordJson {
    pub database: String,
    pub role: String,
    pub peer: String,
    pub publication: String,
    pub subscription: String,
    pub slot: String,
    pub repl_role: String,
    pub state: String,
    pub message: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl From<&crate::replication::ReplRecord> for RecordJson {
    fn from(r: &crate::replication::ReplRecord) -> Self {
        Self {
            database: r.database.clone(),
            role: r.role.as_str().to_string(),
            peer: r.peer.clone(),
            publication: r.publication.clone(),
            subscription: r.subscription.clone(),
            slot: r.slot.clone(),
            repl_role: r.repl_role.clone(),
            state: r.state.as_str().to_string(),
            message: r.message.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// What a primary can see about its own side of the link.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct PrimaryStatus {
    /// Whether a subscriber is currently attached to the slot. `false` with a
    /// growing `behind_bytes` is the shape that eventually fills the disk.
    pub slot_active: bool,
    /// `reserved` / `extended` / `unreserved` / `lost`. Anything but the first
    /// two means WAL the subscriber still needs is at risk or already gone.
    pub wal_status: Option<String>,
    pub behind_bytes: Option<i64>,
    pub current_lsn: Option<String>,
    pub confirmed_flush_lsn: Option<String>,
    pub sender_state: Option<String>,
    pub write_lag_s: Option<f64>,
    pub flush_lag_s: Option<f64>,
    pub replay_lag_s: Option<f64>,
}

/// What a replica can see about its own side.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct ReplicaStatus {
    pub enabled: bool,
    pub worker_running: bool,
    pub received_lsn: Option<String>,
    pub latest_end_lsn: Option<String>,
    pub last_msg_age_s: Option<f64>,
    pub tables_total: i64,
    pub tables_ready: i64,
}

/// `GET /api/replication/{db}` — the record plus whichever side this node can
/// see, plus whatever the peer reported when it was reachable.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct StatusJson {
    pub record: RecordJson,
    #[serde(default)]
    pub primary: Option<PrimaryStatus>,
    #[serde(default)]
    pub replica: Option<ReplicaStatus>,
    /// Why the peer could not be reached, if it could not. A dead peer renders
    /// as a message on the page, never a hung request.
    #[serde(default)]
    pub peer_error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PromoteResponse {
    pub record: RecordJson,
    /// How many sequences were re-seeded. Logical replication carries no
    /// sequence values, so this is the number that would otherwise have
    /// collided on the first insert after the promote.
    pub sequences_fixed: i64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DetachResponse {
    pub record: RecordJson,
    pub slot_dropped: bool,
    pub publication_dropped: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provision() -> ProvisionReplica {
        ProvisionReplica {
            database: "acme".into(),
            peer: "node_a".into(),
            tenant: Login {
                role: "acme".into(),
                password: "tenantpassword".into(),
            },
            repl: Login {
                role: "acme_pgfcrepl".into(),
                password: "replpassword12".into(),
            },
            primary: PrimaryEndpoint {
                hostaddr: "203.0.113.10".into(),
                port: 6432,
                sslmode: "require".into(),
            },
            publication: "pgfc_pub_acme".into(),
            subscription: "pgfc_sub_acme".into(),
            slot: "pgfc_acme_node_b".into(),
            copy_data: true,
            streaming: true,
            primary_server_version_num: Some(160004),
        }
    }

    /// The cross-node contract: two nodes on different builds must agree, and
    /// a silent mismatch here leaves a pairing half-created on two hosts.
    #[test]
    fn provision_request_round_trips() {
        let a = provision();
        let json = serde_json::to_string(&a).unwrap();
        let b: ProvisionReplica = serde_json::from_str(&json).unwrap();
        assert_eq!(b.database, a.database);
        assert_eq!(b.repl.password, a.repl.password);
        assert_eq!(b.primary, a.primary);
        assert_eq!(b.slot, a.slot);
    }

    /// An older peer that has never heard of the newer optional fields must
    /// still be understood, and must default to the safe//previous behaviour.
    #[test]
    fn optional_fields_default_for_an_older_peer() {
        let minimal = r#"{"database":"acme","peer":"node_a",
            "tenant":{"role":"acme","password":"p"},
            "repl":{"role":"acme_pgfcrepl","password":"q"},
            "primary":{"hostaddr":"10.0.0.1","port":6432,"sslmode":"require"},
            "publication":"p","subscription":"s","slot":"sl"}"#;
        let r: ProvisionReplica = serde_json::from_str(minimal).unwrap();
        assert!(
            r.copy_data,
            "an older peer means the original behaviour: seed the data"
        );
        assert!(r.streaming);
        assert_eq!(r.primary_server_version_num, None);

        let n: NodeInfo =
            serde_json::from_str(r#"{"node":"b","replication_enabled":true}"#).unwrap();
        assert!(!n.tls, "unknown means do not assume TLS");
        assert_eq!(n.server_version_num, None);
    }

    /// Both passwords cross the wire in this one struct; neither may reach a
    /// log through a `{:?}` on it or on anything containing it.
    #[test]
    fn debug_never_prints_a_password() {
        let d = format!("{:?}", provision());
        assert!(!d.contains("tenantpassword"), "{d}");
        assert!(!d.contains("replpassword12"), "{d}");
        assert!(
            d.contains("acme_pgfcrepl"),
            "roles are still identifiable: {d}"
        );
    }
}
