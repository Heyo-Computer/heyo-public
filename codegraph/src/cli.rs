use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

use crate::index::{self, Index};
use crate::languages::SymbolKind;
use crate::output::{self, Format};
use crate::{outline, parse, patch, search, snippet, symbols, watch};

#[derive(Parser, Debug)]
#[command(
    name = "codegraph",
    version,
    about = "Tree-sitter-backed code navigation and patching for agents"
)]
pub struct Cli {
    /// Render output as compact text instead of JSON.
    #[arg(long, global = true)]
    pub text: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build/update the on-disk index at `.codegraph/index.json`.
    Index {
        /// Repo root to index. Defaults to the current directory.
        path: Option<PathBuf>,
        /// Rebuild from scratch instead of reusing the previous index.
        #[arg(long)]
        force: bool,
    },
    /// Run a foreground daemon that incrementally re-indexes on file changes.
    Watch {
        /// Repo root to watch. Defaults to the current directory.
        path: Option<PathBuf>,
        /// Coalesce bursts of file events for this many milliseconds before
        /// reindexing.
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },
    /// List symbols in a single file (functions, classes, etc.).
    Symbols {
        file: PathBuf,
    },
    /// Print a hierarchical outline of a file (signatures only, no bodies).
    Outline {
        file: PathBuf,
    },
    /// Print the source of one symbol or a line range.
    Snippet {
        file: PathBuf,
        /// Symbol name (qualified, e.g. `Foo::bar`, or bare `bar`).
        symbol: Option<String>,
        /// Line range, `start:end` or `start-end`.
        #[arg(long)]
        lines: Option<String>,
    },
    /// Search the index by symbol name or signature substring.
    Search {
        query: String,
        /// Filter by symbol kind (function, class, struct, ...).
        #[arg(long)]
        kind: Option<String>,
        /// Match against the qualified symbol name only (default also searches signatures).
        #[arg(long)]
        name: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Look up a symbol's definition in the index.
    Definition {
        symbol: String,
    },
    /// Find lexical references to a name in indexed files (regex word-boundary scan).
    References {
        symbol: String,
    },
    /// Apply a unified diff to a file.
    Patch {
        file: PathBuf,
        /// Path to a diff file. If absent, the diff is read from stdin.
        #[arg(long)]
        diff: Option<PathBuf>,
        /// Parse and apply in memory only; report success without modifying the file.
        #[arg(long)]
        check: bool,
        /// Allow patching files outside the working directory.
        #[arg(long)]
        allow_outside: bool,
    },
    /// Serve the read-only query tools over stdio as an MCP server.
    ///
    /// Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) and
    /// exposes `search`, `definition`, `outline`, `snippet`, and `references`
    /// against the `.codegraph/index.json` at the current working directory.
    /// Intended to be launched by an agent host (e.g. `claude --mcp-config`),
    /// not run interactively.
    Mcp,
}

pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let fmt = if cli.text { Format::Text } else { Format::Json };
    match cli.command {
        Command::Index { path, force } => cmd_index(path, force, fmt),
        Command::Watch { path, debounce_ms } => cmd_watch(path, debounce_ms),
        Command::Symbols { file } => cmd_symbols(file, fmt),
        Command::Outline { file } => cmd_outline(file, fmt),
        Command::Snippet {
            file,
            symbol,
            lines,
        } => cmd_snippet(file, symbol, lines, fmt),
        Command::Search {
            query,
            kind,
            name,
            limit,
        } => cmd_search(query, kind, name, limit, fmt),
        Command::Definition { symbol } => cmd_definition(symbol, fmt),
        Command::References { symbol } => cmd_references(symbol, fmt),
        Command::Patch {
            file,
            diff,
            check,
            allow_outside,
        } => cmd_patch(file, diff, check, allow_outside, fmt),
        Command::Mcp => crate::mcp::serve(),
    }
}

fn cmd_index(path: Option<PathBuf>, force: bool, fmt: Format) -> Result<ExitCode> {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    let (index, report) = index::build(&root, force)?;
    index.save()?;
    match fmt {
        Format::Json => {
            #[derive(serde::Serialize)]
            struct Out<'a> {
                root: &'a std::path::Path,
                files: usize,
                indexed: usize,
                reused: usize,
                failed: &'a [(String, String)],
                index_path: std::path::PathBuf,
            }
            output::print_json(&Out {
                root: &index.root,
                files: index.files.len(),
                indexed: report.indexed,
                reused: report.reused,
                failed: &report.failed,
                index_path: Index::path_for(&index.root),
            })?;
        }
        Format::Text => {
            println!(
                "indexed {} (parsed {}, reused {}, failed {}) → {}",
                index.files.len(),
                report.indexed,
                report.reused,
                report.failed.len(),
                Index::path_for(&index.root).display()
            );
            for (path, err) in &report.failed {
                eprintln!("  failed: {path}: {err}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_watch(path: Option<PathBuf>, debounce_ms: u64) -> Result<ExitCode> {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    watch::run(watch::WatchOpts {
        root,
        debounce: Duration::from_millis(debounce_ms),
    })?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_symbols(file: PathBuf, fmt: Format) -> Result<ExitCode> {
    let parsed = parse::parse_path(&file)?;
    let symbols = symbols::extract(&parsed);
    match fmt {
        Format::Json => output::print_json(&symbols)?,
        Format::Text => {
            for s in &symbols {
                println!(
                    "{:?}\t{}\t{}-{}\t{}",
                    s.kind, s.qualified, s.start_line, s.end_line, s.signature
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_outline(file: PathBuf, fmt: Format) -> Result<ExitCode> {
    let parsed = parse::parse_path(&file)?;
    let symbols = symbols::extract(&parsed);
    let outline = outline::build(&symbols);
    match fmt {
        Format::Json => output::print_json(&outline)?,
        Format::Text => {
            let mut buf = String::new();
            outline::render_text(&outline, 0, &mut buf);
            print!("{buf}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_snippet(
    file: PathBuf,
    symbol: Option<String>,
    lines: Option<String>,
    fmt: Format,
) -> Result<ExitCode> {
    let parsed = parse::parse_path(&file)?;
    let snip = match (symbol, lines) {
        (Some(sym), None) => {
            let (s, body) = snippet::by_symbol(&parsed, &sym)?;
            snippet::Snippet {
                file: file.display().to_string(),
                symbol: Some(s.qualified),
                start_line: s.start_line,
                end_line: s.end_line,
                source: body,
            }
        }
        (None, Some(range)) => {
            let (start, end, body) = snippet::by_lines(&parsed, &range)?;
            snippet::Snippet {
                file: file.display().to_string(),
                symbol: None,
                start_line: start,
                end_line: end,
                source: body,
            }
        }
        (Some(_), Some(_)) => bail!("pass --lines or a symbol name, not both"),
        (None, None) => bail!("pass a symbol name or --lines start:end"),
    };
    match fmt {
        Format::Json => output::print_json(&snip)?,
        Format::Text => {
            println!(
                "// {} ({}-{})",
                snip.symbol.as_deref().unwrap_or("(lines)"),
                snip.start_line,
                snip.end_line
            );
            println!("{}", snip.source);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_search(
    query: String,
    kind: Option<String>,
    by_name: bool,
    limit: usize,
    fmt: Format,
) -> Result<ExitCode> {
    let root = std::env::current_dir()?.canonicalize()?;
    let index = Index::load(&root)?
        .ok_or_else(|| anyhow!("no index at {}; run `codegraph index` first", Index::path_for(&root).display()))?;
    let kind_filter = match kind.as_deref() {
        Some(k) => Some(SymbolKind::parse(k).with_context(|| format!("unknown kind `{k}`"))?),
        None => None,
    };
    let hits = search::search(
        &index,
        search::SearchOpts {
            query: &query,
            kind: kind_filter,
            by_name,
            limit: Some(limit),
        },
    );
    match fmt {
        Format::Json => output::print_json(&hits)?,
        Format::Text => {
            for h in &hits {
                println!(
                    "{}:{}\t{:?}\t{}\t{}",
                    h.file, h.start_line, h.kind, h.symbol, h.signature
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_definition(symbol: String, fmt: Format) -> Result<ExitCode> {
    let root = std::env::current_dir()?.canonicalize()?;
    let index = Index::load(&root)?
        .ok_or_else(|| anyhow!("no index at {}; run `codegraph index` first", Index::path_for(&root).display()))?;
    let hits = search::definition(&index, &symbol);
    if hits.is_empty() {
        match fmt {
            Format::Json => output::print_json(&hits)?,
            Format::Text => eprintln!("no definition for `{symbol}`"),
        }
        return Ok(ExitCode::from(1));
    }
    match fmt {
        Format::Json => output::print_json(&hits)?,
        Format::Text => {
            for h in &hits {
                println!(
                    "{}:{}\t{:?}\t{}\t{}",
                    h.file, h.start_line, h.kind, h.symbol, h.signature
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_references(symbol: String, fmt: Format) -> Result<ExitCode> {
    let root = std::env::current_dir()?.canonicalize()?;
    let index = Index::load(&root)?
        .ok_or_else(|| anyhow!("no index at {}; run `codegraph index` first", Index::path_for(&root).display()))?;
    let bare = symbol.rsplit("::").next().unwrap_or(&symbol);
    let bare = bare.rsplit('.').next().unwrap_or(bare);
    let hits = search::references(&index, bare, &root);
    match fmt {
        Format::Json => output::print_json(&hits)?,
        Format::Text => {
            for h in &hits {
                println!("{}:{}\t{}", h.file, h.line, h.line_text);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_patch(
    file: PathBuf,
    diff: Option<PathBuf>,
    check: bool,
    allow_outside: bool,
    fmt: Format,
) -> Result<ExitCode> {
    let report = patch::run(patch::PatchOpts {
        file: &file,
        diff_path: diff.as_deref(),
        check_only: check,
        allow_outside,
    })?;
    let ok = report.ok;
    match fmt {
        Format::Json => output::print_json(&report)?,
        Format::Text => {
            if ok {
                println!(
                    "{} {}: {}/{} hunks applied{}",
                    if check { "would patch" } else { "patched" },
                    report.file,
                    report.hunks_applied,
                    report.hunks_total,
                    report
                        .bytes_written
                        .map(|n| format!(" ({n} bytes)"))
                        .unwrap_or_default()
                );
            } else {
                eprintln!(
                    "patch failed for {}: {} ({}/{} hunks applied before failure)",
                    report.file,
                    report.failure.as_deref().unwrap_or("unknown"),
                    report.hunks_applied,
                    report.hunks_total
                );
            }
        }
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}
