//! `art` — the command-line front end.
//!
//! `main` does not return `Result`: a `Debug`-formatted error is not something
//! to show a person. Each failure class gets its own exit code and its own line
//! on stderr.

use artifacts::cli::{self, Cli};
use clap::Parser;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("art=info,artifacts=info")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let cli = Cli::parse();
    if let Err(e) = cli::run(cli).await {
        eprintln!("art: {e}");
        // The underlying cause matters when the top line is just "io error".
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            eprintln!("  caused by: {s}");
            source = s.source();
        }
        std::process::exit(cli::exit_code(&e));
    }
}
