//! `app-obs-dump <path>...` — print parquet partitions as JSON lines.
//!
//! For inspecting what actually landed on disk without a query engine: useful
//! when a dashboard shows nothing and the question is whether the data is
//! missing or the query is wrong. Accepts files or directories; directories are
//! walked recursively, and in-progress temporary files are skipped.

use datafusion::arrow::json::LineDelimitedWriter;
use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: app-obs-dump <file-or-directory>...");
        std::process::exit(2);
    }

    let mut files = Vec::new();
    for arg in &args {
        collect(Path::new(arg), &mut files);
    }
    files.sort();

    if files.is_empty() {
        eprintln!("no parquet files found");
        std::process::exit(1);
    }

    let mut rows = 0;
    for file in &files {
        match dump(file) {
            Ok(n) => rows += n,
            Err(e) => eprintln!("{}: {e}", file.display()),
        }
    }
    eprintln!("{} row(s) from {} file(s)", rows, files.len());
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                collect(&entry.path(), out);
            }
        }
    } else if path.extension().is_some_and(|e| e == "parquet") {
        out.push(path.to_path_buf());
    }
}

fn dump(path: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

    let mut rows = 0;
    let stdout = std::io::stdout();
    let mut writer = LineDelimitedWriter::new(stdout.lock());
    for batch in reader {
        let batch = batch?;
        rows += batch.num_rows();
        writer.write(&batch)?;
    }
    writer.finish()?;
    Ok(rows)
}
