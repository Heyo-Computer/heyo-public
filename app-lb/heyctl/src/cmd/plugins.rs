//! `heyctl plugins` — list, switch on and configure app-lb's built-in plugins.
//!
//! The set of plugins is compiled into app-lb; what these commands change is
//! whether each one runs and with what configuration. A plugin can be enabled
//! and failing at once (it could not reach what it needs), so every write
//! prints `last_error` when there is one rather than reporting bare success.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

use crate::PluginView;
use crate::cmd::Ctx;
use crate::output::{self, Table};

#[derive(Subcommand, Debug)]
pub enum PluginsCmd {
    /// List every plugin and whether it is enabled.
    #[command(alias = "ls")]
    List,
    /// Show one plugin: its configuration and live status.
    Describe { id: String },
    /// Switch a plugin on with its stored configuration.
    Enable { id: String },
    /// Switch a plugin off. Its configuration is kept.
    Disable { id: String },
    /// Replace a plugin's configuration.
    Set(SetArgs),
}

#[derive(Args, Debug)]
pub struct SetArgs {
    pub id: String,
    /// A JSON file holding the configuration object, or `-` for stdin.
    #[arg(long, short = 'f', value_name = "FILE")]
    pub file: PathBuf,
    /// Also enable the plugin. Without this the enabled state is unchanged.
    #[arg(long)]
    pub enable: bool,
}

pub fn run(ctx: &Ctx, cmd: &PluginsCmd) -> Result<()> {
    match cmd {
        PluginsCmd::List => list(ctx),
        PluginsCmd::Describe { id } => describe(ctx, id),
        PluginsCmd::Enable { id } => report(ctx.client.set_plugin(id, true, None)?),
        PluginsCmd::Disable { id } => report(ctx.client.set_plugin(id, false, None)?),
        PluginsCmd::Set(args) => set(ctx, args),
    }
}

fn state_cell(p: &PluginView) -> &'static str {
    match (p.enabled, p.last_error.is_some()) {
        (true, true) => "error",
        (true, false) => "enabled",
        (false, _) => "disabled",
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    if ctx.out.is_machine() {
        let raw = ctx.client.raw().plugins()?;
        let names: Vec<String> = raw
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("id").and_then(Value::as_str))
                    .map(|id| format!("plugin/{id}"))
                    .collect()
            })
            .unwrap_or_default();
        return output::emit(&raw, ctx.out, &names);
    }
    let plugins = ctx.client.plugins()?;
    if plugins.is_empty() {
        println!("This app-lb has no plugins.");
        return Ok(());
    }
    let mut table = Table::new(["ID", "NAME", "STATE", "DESCRIPTION"]);
    for p in &plugins {
        table.row([
            p.id.clone(),
            p.name.clone(),
            state_cell(p).to_string(),
            p.description.clone(),
        ]);
    }
    table.print();
    Ok(())
}

fn describe(ctx: &Ctx, id: &str) -> Result<()> {
    if ctx.out.is_machine() {
        let raw = ctx.client.raw().plugins()?;
        let one = raw
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
            })
            .cloned();
        let Some(one) = one else {
            bail!("no plugin named {id:?}")
        };
        return output::emit(&one, ctx.out, &[format!("plugin/{id}")]);
    }
    print_plugin(&ctx.client.plugin(id)?);
    Ok(())
}

fn set(ctx: &Ctx, args: &SetArgs) -> Result<()> {
    let text = if args.file.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading configuration from stdin")?
    } else {
        std::fs::read_to_string(&args.file)
            .with_context(|| format!("reading {}", args.file.display()))?
    };
    let config: Value =
        serde_json::from_str(&text).context("the configuration is not valid JSON")?;
    let enabled = args.enable || ctx.client.plugin(&args.id)?.enabled;
    report(ctx.client.set_plugin(&args.id, enabled, Some(&config))?)
}

/// A write succeeded if the record was saved; say so, and say loudly if
/// applying it did not.
fn report(p: PluginView) -> Result<()> {
    match &p.last_error {
        Some(e) if p.enabled => {
            eprintln!("{} is enabled, but it failed to start: {e}", p.id);
        }
        _ => eprintln!(
            "{} {}.",
            p.id,
            if p.enabled { "enabled" } else { "disabled" }
        ),
    }
    Ok(())
}

fn print_plugin(p: &PluginView) {
    output::section("Plugin");
    output::field("ID", &p.id);
    output::field("Name", &p.name);
    output::field("State", state_cell(p));
    output::field("Description", &p.description);
    if let Some(e) = &p.last_error {
        output::field("Last error", e);
    }
    output::section("Configuration");
    println!(
        "{}",
        serde_json::to_string_pretty(&p.config).unwrap_or_default()
    );
    output::section("Status");
    println!(
        "{}",
        serde_json::to_string_pretty(&p.status).unwrap_or_default()
    );
}
