//! `Sandbox::find_by_name` against two stub daemons: one that honors the
//! `?name=` filter (new heyvmd) and one that ignores it and returns the full
//! inventory (old heyvmd). The SDK must resolve correctly against both, and
//! `info()` must hit the per-id route instead of the listing.

use std::sync::{Arc, Mutex};

use heyo_sdk::{HeyoClientOptions, Sandbox, SandboxStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sandbox_json(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "status": "running",
        "image": "test-image",
        "status_changed_at": "2026-08-19T00:00:00Z",
    })
}

/// Requests seen by the stub, as `path?query` strings.
type SeenRequests = Arc<Mutex<Vec<String>>>;

/// Serve HTTP/1.1 on an ephemeral port. `honor_name_filter` mimics the new
/// daemon (filters the listing by exact name); off mimics the old one (full
/// list regardless of query). `GET /deployed-sandboxes/{id}` answers per-id.
async fn spawn_stub(honor_name_filter: bool) -> (String, SeenRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen: SeenRequests = Arc::new(Mutex::new(Vec::new()));
    let seen_writer = Arc::clone(&seen);

    let fleet = vec![
        ("sb-aaa", "pg-alpha"),
        ("sb-bbb", "pg-beta"),
        ("sb-ccc", "pg-gamma"),
    ];

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen_writer);
            let fleet = fleet.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until end of headers; these are body-less GETs.
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&buf);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push(target.clone());

                let (path, query) = match target.split_once('?') {
                    Some((p, q)) => (p, Some(q)),
                    None => (target.as_str(), None),
                };
                let body = if path == "/deployed-sandboxes" {
                    let name_param = query.and_then(|q| {
                        q.split('&').find_map(|pair| {
                            let (k, v) = pair.split_once('=')?;
                            (k == "name").then(|| v.to_string())
                        })
                    });
                    let entries: Vec<serde_json::Value> = match name_param {
                        Some(name) if honor_name_filter => fleet
                            .iter()
                            .filter(|(_, n)| *n == name)
                            .map(|(id, n)| sandbox_json(id, n))
                            .collect(),
                        _ => fleet.iter().map(|(id, n)| sandbox_json(id, n)).collect(),
                    };
                    serde_json::to_string(&entries).unwrap()
                } else if let Some(id) = path.strip_prefix("/deployed-sandboxes/") {
                    match fleet.iter().find(|(fid, _)| *fid == id) {
                        Some((id, n)) => sandbox_json(id, n).to_string(),
                        None => {
                            let resp = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                            let _ = stream.write_all(resp.as_bytes()).await;
                            return;
                        }
                    }
                } else {
                    let resp = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://{}", addr), seen)
}

fn options_for(base_url: &str) -> HeyoClientOptions {
    HeyoClientOptions {
        api_key: Some("test-key".to_string()),
        base_url: Some(base_url.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn find_by_name_against_filtering_daemon() {
    let (base_url, seen) = spawn_stub(true).await;

    let found = Sandbox::find_by_name("pg-beta", options_for(&base_url))
        .await
        .expect("find_by_name");
    let found = found.expect("pg-beta exists");
    assert_eq!(found.id, "sb-bbb");
    assert_eq!(found.status, SandboxStatus::Running);

    let missing = Sandbox::find_by_name("pg-nope", options_for(&base_url))
        .await
        .expect("find_by_name miss");
    assert!(missing.is_none());

    let requests = seen.lock().unwrap().clone();
    assert!(
        requests.iter().all(|r| r.contains("name=")),
        "every request must carry the name filter: {requests:?}"
    );
}

#[tokio::test]
async fn find_by_name_against_old_daemon_full_list_fallback() {
    let (base_url, _seen) = spawn_stub(false).await;

    // The old daemon ignores the query and returns the whole fleet; the
    // client-side filter must still land on the exact match.
    let found = Sandbox::find_by_name("pg-gamma", options_for(&base_url))
        .await
        .expect("find_by_name");
    assert_eq!(found.expect("pg-gamma exists").id, "sb-ccc");

    let missing = Sandbox::find_by_name("pg-nope", options_for(&base_url))
        .await
        .expect("find_by_name miss");
    assert!(missing.is_none());
}

#[tokio::test]
async fn info_resolves_per_id_not_via_listing() {
    let (base_url, seen) = spawn_stub(true).await;

    let sandbox = Sandbox::connect("sb-aaa".to_string(), options_for(&base_url)).expect("connect");
    let info = sandbox.info().await.expect("info");
    assert_eq!(info.id, "sb-aaa");
    assert_eq!(info.name, "pg-alpha");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests, vec!["/deployed-sandboxes/sb-aaa".to_string()]);
}
