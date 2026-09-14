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

#[tokio::test]
#[ignore = "requires PG_FC_FENCE_TEST_URL pointing to disposable PostgreSQL 18"]
async fn postgres_selective_fence_drains_tenant_and_captures_real_sequences() {
    let (mut config, maintenance, database) = database().await;
    let owner = format!("{database}_owner");
    let repl = format!("{database}_repl");
    maintenance.batch_execute(&format!(
        "CREATE ROLE {owner} LOGIN PASSWORD 'disposable-fence-test'; CREATE ROLE {repl} LOGIN REPLICATION PASSWORD 'disposable-fence-test'; ALTER DATABASE {database} OWNER TO {owner}"
    )).await.unwrap();
    config.dbname(&database).user(&owner).options("-c synchronous_commit=off");
    let tenant = connect(&config).await;
    let mut other_config = config.clone();
    other_config.dbname("postgres");
    let other_database_session = connect(&other_config).await;
    tenant.batch_execute("CREATE SEQUENCE public.cached CACHE 20; CREATE SEQUENCE public.uncalled; SELECT setval('public.uncalled', 73, false)").await.unwrap();
    tenant.batch_execute("BEGIN; SELECT nextval('public.cached'); ROLLBACK").await.unwrap();
    let mut controller_config = maintenance_config();
    controller_config.dbname(&database);
    let mut controller = connect(&controller_config).await;
    let controller_pid: i32 = controller.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);

    let (barrier, snapshots) = fence_postgres_selective(
        &maintenance, &mut controller, &database, &owner, &repl, "unused_test_slot",
        controller_pid, |_, _| Ok(())
    ).await.unwrap();
    assert!(tenant.query_one("SELECT 1", &[]).await.is_err(), "pre-existing tenant survived");
    assert!(other_database_session.query_one("SELECT 1", &[]).await.is_err(), "tenant session in another database survived");
    assert!(config.connect(NoTls).await.is_err(), "NOLOGIN owner reconnected");
    let mut repl_config = maintenance_config();
    repl_config.dbname(&database).user(&repl);
    let repl_client = connect(&repl_config).await;
    assert_eq!(repl_client.query_one("SELECT 1", &[]).await.unwrap().get::<_, i32>(0), 1);
    let cached = snapshots.iter().find(|s| s.name == "cached").unwrap();
    assert_eq!(cached.last_value, 20, "rolled-back/cached allocation must not be derived from table MAX");
    assert!(cached.is_called);
    let uncalled = snapshots.iter().find(|s| s.name == "uncalled").unwrap();
    assert_eq!((uncalled.last_value, uncalled.is_called), (73, false));
    let flushed: bool = maintenance.query_one("SELECT pg_current_wal_flush_lsn() >= $1::text::pg_lsn", &[&barrier]).await.unwrap().get(0);
    assert!(flushed);

    drop(repl_client);
    drop(controller);
    maintenance.batch_execute(&sql::restore_selective_admission(&database, &owner)).await.unwrap();
    maintenance.batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)")).await.unwrap();
    maintenance.batch_execute(&format!("DROP ROLE {owner}; DROP ROLE {repl}")).await.unwrap();
}

#[tokio::test]
#[ignore = "requires PG_FC_FENCE_TEST_URL pointing to disposable PostgreSQL 18"]
async fn postgres_selective_fence_detects_set_role_escape() {
    let (_config, maintenance, database) = database().await;
    let owner = format!("{database}_owner");
    let escape = format!("{database}_escape");
    maintenance.batch_execute(&format!("CREATE ROLE {owner} LOGIN; CREATE ROLE {escape} LOGIN; GRANT {owner} TO {escape} WITH SET TRUE")).await.unwrap();
    let found: Vec<String> = maintenance.query(sql::TENANT_ROLE_ESCAPE_SQL, &[&owner]).await.unwrap().iter().map(|r| r.get(0)).collect();
    assert_eq!(found, vec![escape.clone()]);
    maintenance.batch_execute(&format!("GRANT {owner} TO {escape} WITH SET FALSE, INHERIT TRUE")).await.unwrap();
    let found: Vec<String> = maintenance.query(sql::TENANT_ROLE_ESCAPE_SQL, &[&owner]).await.unwrap().iter().map(|r| r.get(0)).collect();
    assert_eq!(found, vec![escape.clone()], "inherited ownership bypasses a SET-only check");
    maintenance.batch_execute(&format!("REVOKE {owner} FROM {escape}")).await.unwrap();
    let unsupported = validate_selective_roles(&maintenance, &owner, "absent_repl").await.unwrap_err();
    assert!(unsupported.to_string().contains(&escape), "an independent login with table grants must also prevent fencing");
    maintenance.batch_execute(&format!("DROP DATABASE {database}")).await.unwrap();
    maintenance.batch_execute(&format!("DROP OWNED BY {escape}; DROP ROLE {escape}; DROP ROLE {owner}")).await.unwrap();
}

#[tokio::test]
#[ignore = "restarts disposable container named by PG_FC_FENCE_RESTART_CONTAINER; run serially"]
async fn postgres_selective_fence_survives_postgres_restart() {
    let container = std::env::var("PG_FC_FENCE_RESTART_CONTAINER").expect("explicit disposable container required");
    assert!(container.starts_with("heyo-pg-fence-"), "refusing to restart a non-test container");
    let (mut config, maintenance, database) = database().await;
    let owner = format!("{database}_owner");
    let repl = format!("{database}_repl");
    maintenance.batch_execute(&format!(
        "CREATE ROLE {owner} LOGIN PASSWORD 'disposable-fence-test'; CREATE ROLE {repl} LOGIN REPLICATION PASSWORD 'disposable-fence-test'; ALTER DATABASE {database} OWNER TO {owner}"
    )).await.unwrap();
    config.dbname(&database);
    let mut controller = connect(&config).await;
    controller.batch_execute("CREATE SEQUENCE durable CACHE 20; SELECT nextval('durable')").await.unwrap();
    let pid: i32 = controller.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    let (_, sequences) = fence_postgres_selective(&maintenance, &mut controller, &database, &owner, &repl, "unused", pid, |_, _| Ok(())).await.unwrap();
    assert_eq!(sequences[0].last_value, 20);
    drop(controller);
    drop(maintenance);
    let status = tokio::process::Command::new("docker").args(["restart", &container]).status().await.unwrap();
    assert!(status.success());
    let config_maintenance = maintenance_config();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let maintenance = loop {
        match config_maintenance.connect(NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move { let _ = connection.await; });
                break client;
            }
            Err(_) => {
                assert!(tokio::time::Instant::now() < deadline, "disposable PostgreSQL did not restart");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    validate_selective_roles(&maintenance, &owner, &repl).await.unwrap();
    config.user(&owner);
    assert!(config.connect(NoTls).await.is_err(), "restart restored owner LOGIN");
    config.user(&repl);
    let replication_client = connect(&config).await;
    assert_eq!(replication_client.query_one("SELECT 1", &[]).await.unwrap().get::<_, i32>(0), 1);
    drop(replication_client);
    config.user("postgres");
    let controller = connect(&config).await;
    let value: i64 = controller.query_one("SELECT last_value FROM durable", &[]).await.unwrap().get(0);
    assert_eq!(value, 20, "checkpoint did not persist the captured allocation");
    drop(controller);
    maintenance.batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)")).await.unwrap();
    maintenance.batch_execute(&format!("DROP ROLE {owner}; DROP ROLE {repl}")).await.unwrap();
}
