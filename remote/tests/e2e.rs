//! End to end: the real binary, the real `git` client, and a directory store
//! shared by two instances standing in for two regions.
//!
//! Set `REMOTE_TEST_S3_ENDPOINT` (plus `REMOTE_TEST_S3_ACCESS_KEY_ID` /
//! `REMOTE_TEST_S3_SECRET_ACCESS_KEY`) to run the same flow against an
//! S3-compatible store such as MinIO instead.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const ADMIN: &str = "test-operator-token";

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(store_env: &[(String, String)], cache: &Path, prefix: &str) -> Server {
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_remote"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("REMOTE_LISTEN", format!("127.0.0.1:{port}"))
        .env("REMOTE_PUBLIC_URL", &url)
        .env("REMOTE_ADMIN_TOKEN", ADMIN)
        .env("REMOTE_DEFAULT_ACCOUNT", "acct-test")
        .env("REMOTE_BUCKET_PREFIX", prefix)
        .env("REMOTE_CACHE_DIR", cache)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null());
    for (k, v) in store_env {
        cmd.env(k, v);
    }
    #[allow(clippy::zombie_processes)] // reaped by `Server`'s Drop
    let child = cmd.spawn().expect("spawn remote");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server { child, url };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("remote did not start");
}

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (good, text) = git(dir, args);
    assert!(good, "git {args:?} failed:\n{text}");
    text
}

async fn call(method: &str, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let c = reqwest::Client::new();
    let mut req = c.request(method.parse().unwrap(), url).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn with_creds(url: &str, token: &str) -> String {
    url.replacen("http://", &format!("http://x-access-token:{token}@"), 1)
}

fn store_env(root: &Path) -> Vec<(String, String)> {
    match std::env::var("REMOTE_TEST_S3_ENDPOINT") {
        Ok(ep) => vec![
            ("REMOTE_STORE".into(), "s3".into()),
            ("REMOTE_S3_ENDPOINT".into(), ep),
            // Exercise the AWS bucket hardening calls too.
            ("REMOTE_S3_HARDEN".into(), "1".into()),
            (
                "REMOTE_S3_ACCESS_KEY_ID".into(),
                std::env::var("REMOTE_TEST_S3_ACCESS_KEY_ID")
                    .unwrap_or_else(|_| "minioadmin".into()),
            ),
            (
                "REMOTE_S3_SECRET_ACCESS_KEY".into(),
                std::env::var("REMOTE_TEST_S3_SECRET_ACCESS_KEY")
                    .unwrap_or_else(|_| "minioadmin".into()),
            ),
        ],
        Err(_) => vec![("REMOTE_STORE".into(), format!("fs:{}", root.display()))],
    }
}

#[tokio::test]
async fn push_clone_commit_and_race_across_two_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let env = store_env(&tmp.path().join("store"));
    // Unique per run so a shared MinIO starts clean.
    let prefix = format!("t{}", rand::random::<u32>());
    let a = start(&env, &tmp.path().join("cache-a"), &prefix);
    let b = start(&env, &tmp.path().join("cache-b"), &prefix);

    // Create a repo and a write token on instance A.
    let (st, repo) = call(
        "POST",
        &format!("{}/api/repos/team-a", a.url),
        ADMIN,
        Some(json!({"name": "site"})),
    )
    .await;
    assert_eq!(st, 201, "{repo}");
    let clone_url = repo["clone_url"].as_str().unwrap().to_string();
    let (st, tok) = call(
        "POST",
        &format!("{}/api/tokens", a.url),
        ADMIN,
        Some(json!({"namespace": "team-a", "repos": ["site"], "access": "write", "name": "agent"})),
    )
    .await;
    assert_eq!(st, 201, "{tok}");
    let token = tok["token"].as_str().unwrap().to_string();
    let (_, read) = call(
        "POST",
        &format!("{}/api/tokens", a.url),
        ADMIN,
        Some(json!({"namespace": "team-a", "access": "read"})),
    )
    .await;
    let read_token = read["token"].as_str().unwrap().to_string();

    // An unauthenticated clone is asked for credentials, not served.
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (good, text) = git(&work, &["clone", &clone_url, "anon"]);
    assert!(!good, "anonymous clone must fail: {text}");

    // Push a fresh project (no prior repo) to A.
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("index.html"), "<h1>hello</h1>\n").unwrap();
    ok(&proj, &["init", "-q", "-b", "main"]);
    ok(&proj, &["add", "."]);
    ok(&proj, &["commit", "-q", "-m", "first"]);
    ok(
        &proj,
        &["push", &with_creds(&clone_url, &token), "HEAD:main"],
    );

    // A read token cannot push.
    let (good, text) = git(
        &proj,
        &["push", &with_creds(&clone_url, &read_token), "HEAD:other"],
    );
    assert!(!good && text.contains("403"), "{text}");

    // Clone through B, whose cache is empty: the store is the authority.
    let b_url = clone_url.replace(&a.url, &b.url);
    ok(
        &work,
        &["clone", "-q", &with_creds(&b_url, &read_token), "via-b"],
    );
    assert_eq!(
        std::fs::read_to_string(work.join("via-b/index.html")).unwrap(),
        "<h1>hello</h1>\n"
    );

    // Two clients, both based on `first`, push to different instances; the
    // second is refused and nothing is lost.
    let c1 = work.join("c1");
    let c2 = work.join("c2");
    ok(
        &work,
        &["clone", "-q", &with_creds(&clone_url, &token), "c1"],
    );
    ok(&work, &["clone", "-q", &with_creds(&b_url, &token), "c2"]);
    std::fs::write(c1.join("a.txt"), "1").unwrap();
    ok(&c1, &["add", "."]);
    ok(&c1, &["commit", "-q", "-m", "c1"]);
    std::fs::write(c2.join("b.txt"), "2").unwrap();
    ok(&c2, &["add", "."]);
    ok(&c2, &["commit", "-q", "-m", "c2"]);
    ok(&c1, &["push", "-q", "origin", "HEAD:main"]);
    let (good, text) = git(&c2, &["push", "origin", "HEAD:main"]);
    assert!(!good, "the stale push must be refused: {text}");
    // After fetching and rebasing it goes through B.
    ok(&c2, &["pull", "-q", "--rebase", "origin", "main"]);
    ok(&c2, &["push", "-q", "origin", "HEAD:main"]);

    // A commit through the API, on B, with no git on the client.
    let (st, res) = call(
        "POST",
        &format!("{}/api/repos/team-a/site/commits", b.url),
        &token,
        Some(json!({
            "message": "agent upload",
            "files": [
                {"path": "index.html", "content": "<h1>updated</h1>\n"},
                {"path": "assets/logo.bin", "content": "AAEC", "encoding": "base64"},
                {"path": "a.txt", "delete": true}
            ]
        })),
    )
    .await;
    assert_eq!(st, 200, "{res}");
    assert_eq!(res["changed"], true);
    let commit = res["commit"].as_str().unwrap().to_string();

    // A stale base is a 409, not a silent overwrite.
    let (st, res) = call(
        "POST",
        &format!("{}/api/repos/team-a/site/commits", a.url),
        &token,
        Some(
            json!({"message": "x", "base": "0000000000000000000000000000000000000001",
                    "files": [{"path": "x", "content": "x"}]}),
        ),
    )
    .await;
    assert_eq!(st, 409, "{res}");

    // A sees B's commit.
    ok(&c1, &["pull", "-q", "origin", "main"]);
    assert_eq!(ok(&c1, &["rev-parse", "HEAD"]).trim(), commit);
    assert_eq!(
        std::fs::read_to_string(c1.join("index.html")).unwrap(),
        "<h1>updated</h1>\n"
    );
    assert_eq!(
        std::fs::read(c1.join("assets/logo.bin")).unwrap(),
        vec![0, 1, 2]
    );
    assert!(!c1.join("a.txt").exists());
    assert!(c1.join("b.txt").exists());

    // Repo info, listing, and the token wall.
    let (st, info) = call(
        "GET",
        &format!("{}/api/repos/team-a/site", a.url),
        &read_token,
        None,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(info["refs"]["refs/heads/main"], commit);
    let (st, _) = call(
        "GET",
        &format!("{}/api/repos/team-b", a.url),
        &read_token,
        None,
    )
    .await;
    assert_eq!(st, 403);
    let (st, list) = call("GET", &format!("{}/api/repos/team-a", a.url), ADMIN, None).await;
    assert_eq!(st, 200);
    assert_eq!(list["repos"].as_array().unwrap().len(), 1);

    // Delete through A; B no longer serves it.
    let (st, _) = call(
        "DELETE",
        &format!("{}/api/repos/team-a/site", a.url),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(st, 200);
    let (good, _) = git(
        &work,
        &["clone", "-q", &with_creds(&b_url, &read_token), "gone"],
    );
    assert!(!good);
}
