fn main() {
    println!("cargo:rerun-if-env-changed=HEYO_BUILD_GIT_SHA");
    let revision = std::env::var("HEYO_BUILD_GIT_SHA").unwrap_or_else(|_| "unknown".into());
    assert!(revision == "unknown" || (matches!(revision.len(), 40 | 64)
        && revision.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "HEYO_BUILD_GIT_SHA must be a full lowercase Git revision");
    println!("cargo:rustc-env=APP_LB_BUILD_REVISION={revision}");
}
