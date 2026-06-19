use serde::Serialize;

use crate::index::Index;
use crate::languages::SymbolKind;

#[derive(Serialize, Debug)]
pub struct SearchHit {
    pub file: String,
    pub symbol: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
}

#[derive(Default, Debug)]
pub struct SearchOpts<'a> {
    pub query: &'a str,
    pub kind: Option<SymbolKind>,
    pub by_name: bool,
    pub limit: Option<usize>,
}

pub fn search(index: &Index, opts: SearchOpts<'_>) -> Vec<SearchHit> {
    let q = opts.query.to_lowercase();
    let mut hits = Vec::new();
    for (path, entry) in &index.files {
        for sym in &entry.symbols {
            if let Some(k) = opts.kind {
                if sym.kind != k {
                    continue;
                }
            }
            let target = if opts.by_name {
                sym.qualified.to_lowercase()
            } else {
                format!(
                    "{} {}",
                    sym.qualified.to_lowercase(),
                    sym.signature.to_lowercase()
                )
            };
            if q.is_empty() || target.contains(&q) {
                hits.push(SearchHit {
                    file: path.clone(),
                    symbol: sym.qualified.clone(),
                    kind: sym.kind,
                    start_line: sym.start_line,
                    end_line: sym.end_line,
                    signature: sym.signature.clone(),
                });
                if let Some(limit) = opts.limit {
                    if hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }
    }
    hits
}

/// Look up a symbol's definition(s) by exact qualified or bare name. Shared by
/// the `definition` CLI subcommand and the MCP server.
pub fn definition(index: &Index, symbol: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for (path, entry) in &index.files {
        for s in &entry.symbols {
            if s.qualified == symbol || s.name == symbol {
                hits.push(SearchHit {
                    file: path.clone(),
                    symbol: s.qualified.clone(),
                    kind: s.kind,
                    start_line: s.start_line,
                    end_line: s.end_line,
                    signature: s.signature.clone(),
                });
            }
        }
    }
    hits
}

#[derive(Serialize, Debug)]
pub struct ReferenceHit {
    pub file: String,
    pub line: u32,
    pub line_text: String,
    pub resolution: &'static str,
}

/// Lexical (regex word-boundary) reference scan over the indexed files.
/// Tree-sitter-grade reference resolution is out of scope for v1.
pub fn references(index: &Index, name: &str, root: &std::path::Path) -> Vec<ReferenceHit> {
    let mut hits = Vec::new();
    for (rel, _entry) in &index.files {
        let path = root.join(rel);
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in src.lines().enumerate() {
            if word_match(line, name) {
                hits.push(ReferenceHit {
                    file: rel.clone(),
                    line: (i + 1) as u32,
                    line_text: line.to_string(),
                    resolution: "lexical",
                });
            }
        }
    }
    hits
}

fn word_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0;
    while i + n.len() <= bytes.len() {
        if &bytes[i..i + n.len()] == n {
            let prev_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let next_ok = i + n.len() == bytes.len() || !is_word_byte(bytes[i + n.len()]);
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
