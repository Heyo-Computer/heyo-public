//! Run only against a disposable PostgreSQL 18 server with
//! max_prepared_transactions > 0 and synchronous_commit=off.
use super::*;
use tokio_postgres::{Client, Config, NoTls};

async fn connect(config: &Config) -> Client {
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(async move { let _ = connection.await; });
    client
}

async fn database() -> (Config, Client, String) {
    let url = std::env::var("PG_FC_FENCE_TEST_URL")
        .expect("PG_FC_FENCE_TEST_URL must name a disposable PostgreSQL 18 server");
    let config: Config = url.parse().unwrap();
    assert_eq!(config.get_dbname(), Some("postgres"));
    let maintenance = connect(&config).await;
    let version: i32 = maintenance.query_one("SELECT current_setting('server_version_num')::int", &[]).await.unwrap().get(0);
    assert!(version >= 180000, "this regression targets PostgreSQL 18 startup ordering");
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("fence_test_{}_{id}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    maintenance.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
    (config, maintenance, name)
}

#[tokio::test]
#[ignore = "requires PG_FC_FENCE_TEST_URL pointing to disposable PostgreSQL 18"]
async fn postgres_fence_drains_startup_and_preserves_only_committed_data() {
    let (mut config, mut maintenance, database) = database().await;
    config.dbname(&database);
    let app = connect(&config).await;
    app.batch_execute("SET synchronous_commit=off; CREATE TABLE proof(id int PRIMARY KEY); INSERT INTO proof VALUES (11)").await.unwrap();
    app.batch_execute("BEGIN; INSERT INTO proof VALUES (29)").await.unwrap();

    // This delay occurs AFTER datallowconn is checked and BEFORE datid is
    // published in pg_stat_activity. The database-object lock remains visible.
    config.options("-c post_auth_delay=5");
    let startup = tokio::spawn(async move {
        match config.connect(NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move { let _ = connection.await; });
                Ok(client)
            }
            Err(error) => Err(error),
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let starting_pid = loop {
        let holders = maintenance.query(sql::DATABASE_OBJECT_LOCKS_SQL, &[&database]).await.unwrap();
        if let Some(holder) = holders.first() {
            let pid: i32 = holder.get(0);
            let invisible: bool = maintenance.query_one("SELECT datid IS NULL FROM pg_stat_activity WHERE pid=$1", &[&pid]).await.unwrap().get(0);
            if invisible { break pid; }
        }
        assert!(tokio::time::Instant::now() < deadline, "did not observe admitted but unattributed startup");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let barrier = fence_postgres(&mut maintenance, &database, "unused_test_slot", |_, _| Ok(())).await.unwrap();
    let remains: bool = maintenance.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1)", &[&starting_pid]).await.unwrap().get(0);
    assert!(!remains, "a pre-admitted startup backend escaped the fence");
    if let Ok(client) = startup.await.unwrap() {
        assert!(client.batch_execute("INSERT INTO proof VALUES(47)").await.is_err());
    }
    assert!(app.batch_execute("COMMIT").await.is_err(), "uncommitted application transaction survived");
    let flushed: bool = maintenance.query_one("SELECT pg_current_wal_flush_lsn() >= $1::text::pg_lsn", &[&barrier]).await.unwrap().get(0);
    assert!(flushed);

    let mut rejected = maintenance_config();
    rejected.dbname(&database);
    assert!(rejected.connect(NoTls).await.is_err(), "fenced DB accepted a fresh connection");
    maintenance.batch_execute(&sql::set_allow_connections(&database, true)).await.unwrap();
    let check = connect(&rejected).await;
    let values: Vec<i32> = check.query("SELECT id FROM proof ORDER BY id", &[]).await.unwrap().iter().map(|r| r.get(0)).collect();
    assert_eq!(values, vec![11]);
    drop(check);
    maintenance.batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)")).await.unwrap();
}

struct Receiver(std::process::Child);

impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL 18, test_decoding, and pg_recvlogical on PATH"]
async fn postgres_fence_preserves_the_existing_exact_slot_sender() {
    let (mut config, mut maintenance, database) = database().await;
    config.dbname(&database);
    let app = connect(&config).await;
    app.query_one("SELECT * FROM pg_create_logical_replication_slot($1, 'test_decoding')", &[&database]).await.unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("PG_FC_FENCE_TEST_URL").unwrap()).unwrap();
    url.set_path(&database);
    let receiver = Receiver(std::process::Command::new("pg_recvlogical")
        .args(["-d", url.as_str(), "-S", &database, "--start", "--no-loop", "-f", "/dev/null", "-s", "1"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped())
        .spawn().expect("pg_recvlogical must be installed for this integration test"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let sender: i32 = loop {
        let row = maintenance.query_one("SELECT active_pid FROM pg_replication_slots WHERE slot_name=$1", &[&database]).await.unwrap();
        if let Some(pid) = row.get::<_, Option<i32>>(0) { break pid; }
        assert!(tokio::time::Instant::now() < deadline, "replication sender did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    fence_postgres(&mut maintenance, &database, &database, |_, _| Ok(())).await.unwrap();
    let active: Option<i32> = maintenance.query_one("SELECT active_pid FROM pg_replication_slots WHERE slot_name=$1", &[&database]).await.unwrap().get(0);
    assert_eq!(active, Some(sender), "fence killed its existing replication sender");
    drop(receiver);
    maintenance.batch_execute(&sql::set_allow_connections(&database, true)).await.unwrap();
    maintenance.batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)")).await.unwrap();
}

fn maintenance_config() -> Config {
    std::env::var("PG_FC_FENCE_TEST_URL").unwrap().parse().unwrap()
}

#[tokio::test]
#[ignore = "requires PG_FC_FENCE_TEST_URL pointing to disposable PostgreSQL 18"]
async fn postgres_fence_rejects_prepared_transactions_without_discarding_them() {
    let (mut config, mut maintenance, database) = database().await;
    config.dbname(&database);
    let app = connect(&config).await;
    app.batch_execute(&format!("CREATE TABLE proof(id int); BEGIN; INSERT INTO proof VALUES (73); PREPARE TRANSACTION '{database}'")).await.unwrap();
    let error = fence_postgres(&mut maintenance, &database, "unused_test_slot", |_, _| Ok(())).await.unwrap_err();
    assert!(error.to_string().contains("prepared transaction"), "{error:#}");
    let count: i64 = maintenance.query_one(sql::PREPARED_XACTS_SQL, &[&database]).await.unwrap().get(0);
    assert_eq!(count, 1, "fencing must not discard a prepared transaction");
    let closed: bool = maintenance.query_one("SELECT NOT datallowconn FROM pg_database WHERE datname=$1", &[&database]).await.unwrap().get(0);
    assert!(closed, "failure must retain the database admission fence");
    app.batch_execute(&format!("ROLLBACK PREPARED '{database}'")).await.unwrap();
    maintenance.batch_execute(&sql::set_allow_connections(&database, true)).await.unwrap();
    drop(app);
    maintenance.batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)")).await.unwrap();
}
