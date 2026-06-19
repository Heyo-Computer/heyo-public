use anyhow::Result;
use clap::{Parser, Subcommand};

mod mcp;
mod platform;

use platform::types::{KeyAction, MouseAction};

#[derive(Parser, Debug)]
#[command(name = "computer", version, about = "Desktop automation for agents (Wayland Linux + macOS)")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List connected monitors / displays.
    Outputs {
        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// List visible top-level windows.
    Windows {
        #[arg(long)]
        json: bool,
    },
    /// Capture an output to PNG.
    Screenshot {
        /// Output name (see `outputs`). Defaults to first output.
        #[arg(long)]
        output: Option<String>,
        /// Path to write PNG to. Defaults to stdout.
        #[arg(short = 'o', long = "file")]
        file: Option<String>,
    },
    /// Mouse subcommands.
    Mouse {
        #[command(subcommand)]
        action: MouseAction,
    },
    /// Keyboard subcommands.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Type a literal string of text.
    Type {
        text: String,
        /// Inter-keystroke delay in ms.
        #[arg(long, default_value_t = 8)]
        delay_ms: u64,
    },
    /// Open a URL in the default web browser (fire-and-forget).
    Browse {
        /// http://, https://, or file:// URL to open.
        url: String,
    },
    /// Sleep for milliseconds.
    Sleep { ms: u64 },
    /// Serve the desktop tools over stdio as an MCP server.
    ///
    /// Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) and
    /// exposes screenshot/outputs/windows/mouse/key/type/browse as tools.
    /// `screenshot` returns an inline PNG image. Intended to be launched by an
    /// agent host (e.g. `claude --mcp-config`), not run interactively.
    Mcp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Outputs { json } => platform::outputs::run(json),
        Cmd::Windows { json } => platform::windows::run(json),
        Cmd::Screenshot { output, file } => {
            platform::screenshot::run(output.as_deref(), file.as_deref())
        }
        Cmd::Mouse { action } => platform::input::mouse(action),
        Cmd::Key { action } => platform::input::key(action),
        Cmd::Type { text, delay_ms } => platform::input::type_text(&text, delay_ms),
        Cmd::Browse { url } => platform::browse::run(&url),
        Cmd::Sleep { ms } => {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(())
        }
        Cmd::Mcp => mcp::serve(),
    }
}
