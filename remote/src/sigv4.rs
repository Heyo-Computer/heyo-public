//! AWS Signature Version 4, `Authorization`-header form, for S3.
//!
//! The signing core and helpers are copied from
//! `artifacts/src/remote/sigv4.rs` (itself copied from `pg-fc/src/s3.rs`).
//! Those copies only presign query strings with `host` as the one signed
//! header. This service also needs bucket-level calls (CreateBucket,
//! PutPublicAccessBlock, PutBucketEncryption) whose bodies and `x-amz-*`
//! headers must be signed, so this is the header form: every header handed in
//! is signed, plus `host`, `x-amz-date` and `x-amz-content-sha256`.
//!
//! A shared crate for the three copies is a follow-up. Until then, a fix to the
//! shared helpers here belongs in pg-fc and artifacts as well.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

pub struct SignParams<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Already path-encoded (see [`encode_uri_path`]).
    pub canonical_uri: &'a str,
    /// Raw, unencoded query pairs. A bare flag such as `?publicAccessBlock` is
    /// `("publicAccessBlock", "")`.
    pub query: &'a [(String, String)],
    /// Extra headers to sign and send, names in any case.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the body.
    pub payload_hash: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub unix_secs: u64,
}

/// The headers to send, `Authorization` included, for a signed request.
///
/// Returned as a list rather than applied to a request builder so the signing
/// stays a pure function a fixed input pins (see the AWS vectors in the tests).
pub fn sign(p: SignParams) -> Vec<(String, String)> {
    let (date, amz_date) = format_amz_time(p.unix_secs);
    let scope = format!("{date}/{}/s3/aws4_request", p.region);

    let mut headers: Vec<(String, String)> = p
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.push(("host".into(), p.host.to_string()));
    headers.push(("x-amz-content-sha256".into(), p.payload_hash.to_string()));
    headers.push(("x-amz-date".into(), amz_date.clone()));
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        p.method,
        p.canonical_uri,
        canonical_query(p.query),
        canonical_headers,
        signed_headers,
        p.payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(sha256(canonical_request.as_bytes()))
    );
    let key = signing_key(p.secret_key, &date, p.region, "s3");
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));

    headers.retain(|(k, _)| k != "host");
    headers.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
            p.access_key
        ),
    ));
    headers
}

/// The query string exactly as it was signed, for appending to the URL.
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (encode_uri_component(k), encode_uri_component(v)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Derive the SigV4 signing key: HMAC chain over date → region → service →
/// `aws4_request`, seeded with `"AWS4" + secret`.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Split `scheme://host[:port]` into `("https"|"http", "host[:port]")`.
/// Defaults to `https` and treats the whole string as host if no scheme.
pub fn split_scheme_host(url: &str) -> (&str, &str) {
    let url = url.trim_end_matches('/');
    if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        ("https", url)
    }
}

/// RFC 3986 encode a URI **path**: unreserved chars pass through, `/` is
/// preserved (segment separators), everything else is `%XX`.
pub fn encode_uri_path(s: &str) -> String {
    encode(s, true)
}

/// RFC 3986 encode a query component: like [`encode_uri_path`] but `/` is also
/// escaped (`%2F`).
fn encode_uri_component(s: &str) -> String {
    encode(s, false)
}

fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Format a UNIX timestamp as SigV4's `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` pair, in
/// UTC, with no external date crate. Uses Howard Hinnant's civil-from-days.
pub fn format_amz_time(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);

    let date = format!("{year:04}{month:02}{day:02}");
    let datetime = format!("{date}T{hour:02}{min:02}{sec:02}Z");
    (date, datetime)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn auth_of(headers: &[(String, String)]) -> &str {
        &headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1
    }

    #[test]
    fn matches_aws_documented_get_object_example() {
        // "Example: GET Object" in the S3 SigV4 header-auth documentation.
        let headers = sign(SignParams {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            canonical_uri: "/test.txt",
            query: &[],
            headers: &[("Range".into(), "bytes=0-9".into())],
            payload_hash: EMPTY,
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
            unix_secs: 1_369_353_600,
        });
        assert_eq!(
            auth_of(&headers),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(
            !headers.iter().any(|(k, _)| k == "host"),
            "reqwest sets host itself"
        );
    }

    #[test]
    fn matches_aws_documented_list_objects_example() {
        // "Example: GET Bucket (List Objects)": query params are signed sorted.
        let headers = sign(SignParams {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            canonical_uri: "/",
            query: &[
                ("prefix".into(), "J".into()),
                ("max-keys".into(), "2".into()),
            ],
            headers: &[],
            payload_hash: EMPTY,
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
            unix_secs: 1_369_353_600,
        });
        assert!(
            auth_of(&headers).ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{}",
            auth_of(&headers)
        );
    }

    #[test]
    fn civil_time_formats_utc() {
        assert_eq!(
            format_amz_time(1_709_213_831),
            ("20240229".into(), "20240229T133711Z".into())
        );
    }

    #[test]
    fn query_component_escapes_slash_but_path_keeps_it() {
        assert_eq!(encode_uri_component("a/b"), "a%2Fb");
        assert_eq!(encode_uri_path("a/b c"), "a/b%20c");
        assert_eq!(
            canonical_query(&[("uploads".into(), String::new())]),
            "uploads="
        );
    }
}
