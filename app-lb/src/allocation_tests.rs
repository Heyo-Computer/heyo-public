use super::*;
use heyo_sdk::{DaemonCreateRequest, DaemonMount, HeyoClientOptions, SandboxDriver};
use std::{collections::HashMap, sync::{Arc, atomic::{AtomicUsize, Ordering}}};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener, task::JoinHandle};

fn client(base_url: String, key: Option<&str>) -> HeyoClient {
    HeyoClient::new(HeyoClientOptions {
        base_url: Some(base_url),
        api_key: key.map(str::to_owned),
        timeout: None,
    }).unwrap()
}

fn receipt(intent: &Intent) -> Receipt {
    Receipt {
        operation_id: intent.operation_id.clone(),
        sandbox_id: format!("sb-{}", "a".repeat(32)),
        request_digest: intent.request_digest.clone(),
        backend_request_digest: intent.backend_request_digest.clone(),
    }
}

async fn server(status: &str, body: Vec<u8>, location: Option<&str>, expected_request: Option<String>)
    -> (String, Arc<AtomicUsize>, JoinHandle<()>)
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let status = status.to_owned();
    let location = location.map(str::to_owned);
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 { break; }
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end + 4]);
                    let length = headers.lines().find_map(|line| {
                        line.to_ascii_lowercase().strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    }).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let headers = String::from_utf8_lossy(&request);
            assert!(headers.to_ascii_lowercase().contains("authorization: bearer test-key\r\n"));
            let first = headers.lines().next().unwrap().to_owned();
            if let Some(expected) = &expected_request { assert_eq!(&first, expected); }
            // The parent asserts this count, so a detached assertion failure
            // cannot accidentally satisfy a test expecting an HTTP error.
            seen.fetch_add(1, Ordering::SeqCst);
            let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
            if let Some(value) = &location { response.push_str(&format!("Location: {value}\r\n")); }
            response.push_str("\r\n");
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        }
    });
    (url, count, handle)
}

#[test]
fn native_body_has_only_daemon_defaults_and_adapter_fields() {
    let request = DaemonCreateRequest {
        name: "native".into(),
        driver: Some(SandboxDriver::Firecracker),
        ..Default::default()
    };
    let prepared = prepare(&request, "http://127.0.0.1:1", "account/a").unwrap();
    assert_eq!(prepared.body["kind"], "plain");
    let body = prepared.body["request"].as_object().unwrap();
    assert_eq!(body.get("image"), Some(&json!("ubuntu:24.04")));
    assert_eq!(body.get("size_class"), Some(&json!("small")));
    assert_eq!(body.get("region"), Some(&json!("US")));
    assert_eq!(body.get("open_ports"), Some(&json!([])));
    assert_eq!(body.get("backend_type"), Some(&json!("firecracker")));
    assert!(!body.contains_key("driver"));
    assert!(!body.contains_key("sandbox_type"));
}

#[test]
fn archive_preserves_archive_mount_owner_and_environment_fields() {
    let request = DaemonCreateRequest {
        name: "archive".into(),
        s3_archive_key: Some("workspaces/a.tar.zst".into()),
        sandbox_path: Some("/workspace".into()),
        mounts: vec![DaemonMount::from_tree("tree-1", "/data", true)],
        account_id: Some("acct-1".into()),
        user_id: Some("user-1".into()),
        env_vars: Some(HashMap::from([("TOKEN".into(), "resolved-secret".into())])),
        ..Default::default()
    };
    let prepared = prepare(&request, "http://127.0.0.1:1", "scope").unwrap();
    assert_eq!(prepared.body["kind"], "archive");
    let body = &prepared.body["request"];
    assert_eq!(body["s3_archive_key"], "workspaces/a.tar.zst");
    assert_eq!(body["sandbox_path"], "/workspace");
    assert_eq!(body["mounts"][0], json!({"tree_id":"tree-1","sandbox_path":"/data","read_only":true}));
    assert_eq!(body["account_id"], "acct-1");
    assert_eq!(body["user_id"], "user-1");
    assert_eq!(body["env_vars"]["TOKEN"], "resolved-secret");
    let persisted = serde_json::to_string(&prepared.intent).unwrap();
    assert!(!persisted.contains("resolved-secret"));
    assert!(!persisted.contains("TOKEN"));
}

#[test]
fn canonical_fingerprint_is_order_stable_and_binds_scope_body_and_kind() {
    let a = json!({"env_vars":{"A":"1","B":"2"},"name":"x"});
    let b = json!({"name":"x","env_vars":{"B":"2","A":"1"}});
    let backend = |kind: &str, request: Value| digest(json!({"kind":kind,"request":request}));
    assert_eq!(backend("plain", a.clone()), backend("plain", b));
    assert_ne!(backend("plain", a.clone()), backend("archive", a.clone()));
    assert_ne!(backend("plain", a.clone()), backend("plain", json!({"name":"y"})));
    let d = backend("plain", a);
    assert_ne!(digest(json!({"scope":"one","backendRequestDigest":d})),
               digest(json!({"scope":"two","backendRequestDigest":d})));
}

#[test]
fn receipt_validation_rejects_each_bound_identity_mismatch() {
    let intent = Intent { operation_id:"op".into(), request_digest:"request".into(), backend_request_digest:"backend".into(), transport:"t".into() };
    let valid = receipt(&intent);
    assert!(intent.accepts(&valid));
    for changed in [
        Receipt { operation_id:"other".into(), ..valid.clone() },
        Receipt { request_digest:"other".into(), ..valid.clone() },
        Receipt { backend_request_digest:"other".into(), ..valid.clone() },
        Receipt { sandbox_id:"sb-not-a-valid-identity".into(), ..valid.clone() },
    ] { assert!(!intent.accepts(&changed)); }
}

#[tokio::test]
async fn submit_and_recover_use_exact_authenticated_method_path_and_status_rules() {
    for (recovering, status, succeeds) in [(false, "202 Accepted", true), (true, "200 OK", true), (true, "202 Accepted", false)] {
        let seed = prepare(&DaemonCreateRequest { name:"vm".into(), ..Default::default() }, "http://placeholder", "scope").unwrap();
        let response = serde_json::to_vec(&receipt(&seed.intent)).unwrap();
        let expected = format!("{} /sandbox-creations/{} HTTP/1.1", if recovering { "GET" } else { "POST" }, seed.intent.operation_id);
        let (url, count, handle) = server(status, response, None, Some(expected)).await;
        let mut intent = seed.intent.clone(); intent.transport = url.clone();
        let result = if recovering {
            recover(&client(url.clone(), Some("test-key")), &url, &intent).await
        } else {
            let prepared = Prepared { intent, body: seed.body };
            submit(&client(url.clone(), Some("test-key")), &url, &prepared).await
        };
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        handle.abort();
    }
}

#[tokio::test]
async fn failures_do_not_redirect_retry_fallback_or_send_when_preconditions_fail() {
    let seed = prepare(&DaemonCreateRequest { name:"vm".into(), ..Default::default() }, "http://placeholder", "scope").unwrap();
    let trap = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}/fallback", trap.local_addr().unwrap());
    for (status, body, location) in [
        ("307 Temporary Redirect", b"redirect".to_vec(), Some(destination.as_str())),
        ("200 OK", b"{".to_vec(), None),
        ("200 OK", vec![b'x'; 65_537], None),
    ] {
        let expected = format!("GET /sandbox-creations/{} HTTP/1.1", seed.intent.operation_id);
        let (url, count, handle) = server(status, body, location, Some(expected)).await;
        let mut intent = seed.intent.clone(); intent.transport = url.clone();
        assert!(recover(&client(url.clone(), Some("test-key")), &url, &intent).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    assert!(tokio::time::timeout(std::time::Duration::from_millis(50), trap.accept()).await.is_err());
    let (url, count, handle) = server("200 OK", Vec::new(), None, None).await;
    let mut changed = seed.intent.clone(); changed.transport = "http://different".into();
    assert!(recover(&client(url.clone(), Some("test-key")), &url, &changed).await.is_err());
    assert!(recover(&client(url.clone(), None), &url, &Intent { transport:url.clone(), ..seed.intent }).await.is_err());
    tokio::task::yield_now().await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    handle.abort();
}
