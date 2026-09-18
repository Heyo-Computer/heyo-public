//! Strict, bounded inspection of the published app-lb bundle. Shared with CI so
//! neither admission nor installation trusts a workflow-provided binary digest.
use std::io::Read;
use sha2::{Digest, Sha256};

pub const LIMIT: u64 = 256 * 1024 * 1024;

pub fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn valid_sha(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn executable(bytes: &[u8], revision: &str) -> Result<Vec<u8>, String> {
    if bytes.len() as u64 > LIMIT || !valid_sha(revision, 40) { return Err("invalid bundle size or revision".into()); }
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes).take(LIMIT + 1));
    let mut binary = None;
    let mut stamp = None;
    let mut sums = None;
    let mut total = 0u64;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        total = total.checked_add(entry.size()).ok_or("bundle size overflow")?;
        if total > LIMIT { return Err("bundle expansion exceeds budget".into()); }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if !path.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
            return Err("unsafe bundle path".into());
        }
        if entry.header().entry_type().is_dir() { continue; }
        if !entry.header().entry_type().is_file() { return Err("bundle links and special entries are forbidden".into()); }
        let slot = match path.to_str() {
            Some("dist/app-lb") => &mut binary,
            Some("dist/REVISION") => &mut stamp,
            Some("dist/SHA256SUMS") => &mut sums,
            _ => continue,
        };
        if slot.is_some() { return Err("duplicate bundle identity entry".into()); }
        if path != std::path::Path::new("dist/app-lb") && entry.size() > 65536 {
            return Err("oversized bundle metadata".into());
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        *slot = Some(data);
    }
    if archive.into_inner().limit() == 0 { return Err("bundle expansion exceeds budget".into()); }
    if stamp.as_deref() != Some(format!("{revision}\n").as_bytes()) { return Err("bundle revision mismatch".into()); }
    let binary = binary.ok_or("missing app-lb executable")?;
    if !binary.starts_with(b"\x7fELF") { return Err("app-lb is not ELF".into()); }
    let sums = String::from_utf8(sums.ok_or("missing SHA256SUMS")?).map_err(|e| e.to_string())?;
    let matches: Vec<_> = sums.lines().filter(|l| l.split_whitespace().nth(1) == Some("app-lb")).collect();
    if matches != [format!("{}  app-lb", sha(&binary))] { return Err("app-lb checksum mismatch".into()); }
    Ok(binary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn bundle(revision: &str, duplicate: bool, corrupt: bool) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let binary = b"\x7fELFtest executable";
        let stamp = format!("{revision}\n");
        let checksum = format!("{}  app-lb\n", if corrupt { "0".repeat(64) } else { sha(binary) });
        for (path, bytes) in [("dist/app-lb", binary.as_slice()), ("dist/REVISION", stamp.as_bytes()), ("dist/SHA256SUMS", checksum.as_bytes())]
            .into_iter().chain(duplicate.then_some(("dist/app-lb", binary.as_slice()))) {
            let mut header = tar::Header::new_gnu(); header.set_size(bytes.len() as u64); header.set_mode(0o755); header.set_cksum();
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }
    #[test]
    fn exact_bundle_identity() {
        let revision = "a".repeat(40);
        assert_eq!(executable(&bundle(&revision, false, false), &revision).unwrap(), b"\x7fELFtest executable");
        assert!(executable(&bundle(&revision, true, false), &revision).is_err());
        assert!(executable(&bundle(&revision, false, true), &revision).is_err());
        assert!(executable(&bundle(&revision, false, false), &"b".repeat(40)).is_err());
        assert!(executable(b"corrupt gzip", &revision).is_err());
    }

    #[test]
    fn host_bundle_rejects_traversal_links_and_expansion() {
        for (name, kind, size) in [("../app-lb", b'0', 0), ("dist/app-lb", b'2', 0), ("dist/large", b'0', LIMIT + 1)] {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(tar::EntryType::new(kind)); header.set_size(size); header.set_mode(0o755); header.set_cksum();
            // Deliberately short body: the declared oversized entry must be
            // rejected before reading it, not allocated from an untrusted size.
            tar.append(&header, std::io::empty()).unwrap();
            let bytes = tar.into_inner().unwrap().finish().unwrap();
            assert!(executable(&bytes,&"a".repeat(40)).is_err(),"{name}/{kind}/{size}");
        }
    }
}
