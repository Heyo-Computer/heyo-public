//! `login`, `logout`, `whoami` and `config` — everything about *which* app-lb
//! this CLI talks to and as whom.
//!
//! app-lb authenticates with HTTP Basic and has no token endpoint, so "logging
//! in" means verifying credentials once and storing them in the config file.
//! The two gates it exposes are independent — `APP_LB_DASHBOARD_PASSWORD` gates
//! the dashboard and `/metrics`, `APP_LB_ADMIN_AUTH` extends that to the
//! deployment CRUD API — so login probes both and says which it found.

use crate::GlobalOpts;
use crate::client::{Client, probe_auth};
use crate::config::{Config, ContextEntry, resolve_endpoint};
use crate::output::{self, Table};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::time::Duration;

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// The admin API to talk to, e.g. `http://127.0.0.1:9090`. `host:port` is
    /// accepted and assumed to be http.
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Basic-auth user. app-lb's default is `admin`.
    #[arg(long, value_name = "NAME")]
    pub user: Option<String>,

    /// The password. Prefer --password-stdin or the interactive prompt: an
    /// argument is visible in `ps` and your shell history.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// Read the password from stdin (the trailing newline is stripped).
    #[arg(long, conflicts_with = "password")]
    pub password_stdin: bool,

    /// Store this shell command instead of the password itself; it is run to
    /// fetch the password on each request that needs it.
    #[arg(long, value_name = "CMD", conflicts_with_all = ["password", "password_stdin"])]
    pub password_command: Option<String>,

    /// Verify the credentials but don't write the password to disk — supply it
    /// per invocation via SERVERCTL_PASSWORD.
    #[arg(long)]
    pub no_store_password: bool,

    /// Name for the stored context. Defaults to the server's host.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Accept any TLS certificate. Only for a self-signed admin endpoint you
    /// control.
    #[arg(long)]
    pub insecure_skip_tls_verify: bool,

    /// Store the context without making it the current one.
    #[arg(long)]
    pub no_switch: bool,
}

pub fn login(globals: &GlobalOpts, args: &LoginArgs) -> Result<()> {
    let path = Config::path(globals.config.as_deref())?;
    let mut config = Config::load(&path)?;

    let server = args
        .server
        .clone()
        .or_else(|| globals.server.clone())
        .unwrap_or_else(|| crate::client::DEFAULT_SERVER.to_string());
    let user = args
        .user
        .clone()
        .or_else(|| globals.user.clone())
        .unwrap_or_else(|| "admin".to_string());
    let insecure = args.insecure_skip_tls_verify || globals.insecure_skip_tls_verify;
    let timeout = Duration::from_secs(globals.request_timeout);

    // Reachability first: a typo'd port should not look like a bad password.
    let anon = Client::new(&server, None, None, insecure, timeout)?;
    anon.healthz()
        .with_context(|| format!("cannot reach an app-lb admin API at {server}"))?;
    let gates = probe_auth(&anon)?;

    if !gates.any() {
        println!(
            "{server} has no authentication configured — nothing to log in with.\n\
             (Set APP_LB_DASHBOARD_PASSWORD on the server to gate the dashboard and /metrics, \
             and APP_LB_ADMIN_AUTH=1 to gate the deployment API too.)"
        );
        save_context(
            &mut config,
            &path,
            args,
            ContextEntry {
                server: anon.server().to_string(),
                user: None,
                password: None,
                password_command: None,
                insecure_skip_tls_verify: insecure,
            },
        )?;
        return Ok(());
    }

    // Resolve a password: a command is stored and run, everything else is
    // material we have to hold now to verify it.
    let password = match (&args.password_command, &args.password, args.password_stdin) {
        (Some(cmd), _, _) => run_command(cmd)?,
        (None, Some(p), _) => p.clone(),
        (None, None, true) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .context("reading the password from stdin")?;
            buf.trim_end_matches(['\n', '\r']).to_string()
        }
        (None, None, false) => match globals.password.clone() {
            Some(p) => p,
            None => rpassword::prompt_password(format!("Password for {user}@{server}: "))
                .context("reading the password")?,
        },
    };
    if password.is_empty() {
        bail!("an empty password will not authenticate against app-lb");
    }

    // Verify against whichever surface is actually gated.
    let client = Client::new(&server, Some(&user), Some(&password), insecure, timeout)?;
    let verify_path = if gates.crud_gated { "/deployments" } else { "/metrics" };
    match client.status_of(verify_path)? {
        200 => {}
        401 => bail!("the server rejected these credentials — wrong user or password"),
        code => bail!("unexpected HTTP {code} from {verify_path} while verifying credentials"),
    }

    println!(
        "Logged in to {} as {user}.\n  gated: {}",
        client.server(),
        gate_summary(gates.metrics_gated, gates.crud_gated)
    );
    if args.no_store_password {
        println!("  password not stored — set SERVERCTL_PASSWORD for later commands");
    }

    save_context(
        &mut config,
        &path,
        args,
        ContextEntry {
            server: client.server().to_string(),
            user: Some(user.clone()),
            password: (!args.no_store_password && args.password_command.is_none())
                .then(|| password.clone()),
            password_command: args.password_command.clone(),
            insecure_skip_tls_verify: insecure,
        },
    )
}

fn gate_summary(metrics: bool, crud: bool) -> String {
    match (metrics, crud) {
        (true, true) => "dashboard, /metrics and the deployment API".into(),
        (true, false) => "dashboard and /metrics (the deployment API is open)".into(),
        (false, true) => "the deployment API".into(),
        (false, false) => "nothing".into(),
    }
}

fn save_context(
    config: &mut Config,
    path: &std::path::Path,
    args: &LoginArgs,
    entry: ContextEntry,
) -> Result<()> {
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| context_name_for(&entry.server));
    config.contexts.insert(name.clone(), entry);
    if !args.no_switch {
        config.current_context = Some(name.clone());
    }
    config.save(path)?;
    println!("Context {name:?} saved to {}.", path.display());
    Ok(())
}

/// Derive a context name from the server: `http://lb.example.com:9090` becomes
/// `lb.example.com`, and a loopback address becomes `local`.
fn context_name_for(server: &str) -> String {
    let host = server
        .split("://")
        .nth(1)
        .unwrap_or(server)
        .split('/')
        .next()
        .unwrap_or(server);
    let bare = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    match bare {
        "127.0.0.1" | "localhost" | "::1" | "[::1]" => "local".to_string(),
        other if other.is_empty() => "default".to_string(),
        other => other.to_string(),
    }
}

fn run_command(cmd: &str) -> Result<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)
        .context("the password command produced non-UTF-8 output")?
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// The context to forget. Defaults to the current one.
    #[arg(value_name = "CONTEXT")]
    pub name: Option<String>,

    /// Drop only the stored password, keeping the server and user.
    #[arg(long)]
    pub keep_context: bool,
}

pub fn logout(globals: &GlobalOpts, args: &LogoutArgs) -> Result<()> {
    let path = Config::path(globals.config.as_deref())?;
    let mut config = Config::load(&path)?;

    let name = args
        .name
        .clone()
        .or_else(|| globals.context.clone())
        .or_else(|| config.current_context.clone())
        .context("no context to log out of")?;
    let entry = config
        .contexts
        .get_mut(&name)
        .with_context(|| format!("no context named {name:?}"))?;

    if args.keep_context {
        entry.password = None;
        entry.password_command = None;
        println!("Cleared the stored password for context {name:?}.");
    } else {
        config.contexts.remove(&name);
        if config.current_context.as_deref() == Some(name.as_str()) {
            // Fall back to whatever is left, so the next command still has a
            // target rather than silently reverting to the default server.
            config.current_context = config.contexts.keys().next().cloned();
        }
        println!("Removed context {name:?}.");
    }
    config.save(&path)
}

/// Where this invocation would connect, and what the server would let it do.
pub fn whoami(globals: &GlobalOpts) -> Result<()> {
    let path = Config::path(globals.config.as_deref())?;
    let config = Config::load(&path)?;
    let endpoint = resolve_endpoint(
        &config,
        globals.context.as_deref(),
        globals.server.as_deref(),
        globals.user.as_deref(),
        globals.password.as_deref(),
        globals.insecure_skip_tls_verify,
    )?;

    output::section("Client");
    output::field("Config file", path.display().to_string());
    output::field("Context", &endpoint.name);
    output::field("Server", &endpoint.server);
    output::field("User", endpoint.user.as_deref().unwrap_or("admin (default)"));
    output::field("Password", endpoint.password_source.describe());
    if endpoint.insecure_skip_tls_verify {
        output::field("TLS", "certificate verification disabled");
    }

    let client = Client::new(
        &endpoint.server,
        endpoint.user.as_deref(),
        endpoint.password.as_deref(),
        endpoint.insecure_skip_tls_verify,
        Duration::from_secs(globals.request_timeout),
    )?;

    output::section("Server");
    if let Err(e) = client.healthz() {
        output::field("Reachable", format!("no — {e:#}"));
        return Ok(());
    }
    output::field("Reachable", "yes (GET /healthz)");

    let gates = probe_auth(&client)?;
    output::field("Auth required for", gate_summary(gates.metrics_gated, gates.crud_gated));

    // What this identity can actually do right now, which is the question
    // behind "why am I getting a 401".
    let access = |path: &str| -> String {
        match client.status_of(path) {
            Ok(200) => "allowed".to_string(),
            Ok(401) if client.has_credentials() => "denied (credentials rejected)".to_string(),
            Ok(401) => "denied (no credentials)".to_string(),
            Ok(code) => format!("HTTP {code}"),
            Err(e) => format!("error: {e}"),
        }
    };
    output::field("Deployment API", access("/deployments"));
    output::field("Metrics", access("/metrics"));
    Ok(())
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the config file, with passwords redacted.
    View {
        /// Print passwords in the clear.
        #[arg(long)]
        show_secrets: bool,
    },
    /// List the stored contexts.
    GetContexts,
    /// Print the name of the current context.
    CurrentContext,
    /// Switch the current context.
    UseContext {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Create or update a context.
    SetContext(SetContextArgs),
    /// Remove a context.
    DeleteContext {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Print the path of the config file.
    Path,
}

#[derive(Args, Debug)]
pub struct SetContextArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,
    #[arg(long, value_name = "NAME")]
    pub user: Option<String>,
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,
    /// A shell command whose stdout is the password.
    #[arg(long, value_name = "CMD", conflicts_with = "password")]
    pub password_command: Option<String>,
    #[arg(long)]
    pub insecure_skip_tls_verify: bool,
    /// Make this the current context.
    #[arg(long)]
    pub current: bool,
}

pub fn config(globals: &GlobalOpts, cmd: &ConfigCmd) -> Result<()> {
    let path = Config::path(globals.config.as_deref())?;
    let mut config = Config::load(&path)?;

    match cmd {
        ConfigCmd::Path => println!("{}", path.display()),

        ConfigCmd::View { show_secrets } => {
            let mut view = serde_json::to_value(&config)?;
            if !show_secrets
                && let Some(contexts) = view.get_mut("contexts").and_then(|c| c.as_object_mut())
            {
                for (_, entry) in contexts.iter_mut() {
                    if let Some(obj) = entry.as_object_mut()
                        && obj.contains_key("password")
                    {
                        obj["password"] = serde_json::Value::String("<redacted>".into());
                    }
                }
            }
            println!("{}", serde_json::to_string_pretty(&view)?);
        }

        ConfigCmd::GetContexts => {
            if config.contexts.is_empty() {
                println!(
                    "No contexts. `serverctl login` creates one; without it, commands go to {}.",
                    crate::client::DEFAULT_SERVER
                );
                return Ok(());
            }
            let mut table = Table::new(["CURRENT", "NAME", "SERVER", "USER", "PASSWORD"]);
            for (name, entry) in &config.contexts {
                table.row([
                    if config.current_context.as_deref() == Some(name.as_str()) {
                        "*".to_string()
                    } else {
                        String::new()
                    },
                    name.clone(),
                    entry.server.clone(),
                    entry.user.clone().unwrap_or_else(|| "—".into()),
                    match (&entry.password, &entry.password_command) {
                        (_, Some(_)) => "command".to_string(),
                        (Some(_), None) => "stored".to_string(),
                        (None, None) => "—".to_string(),
                    },
                ]);
            }
            table.print();
        }

        ConfigCmd::CurrentContext => match &config.current_context {
            Some(name) => println!("{name}"),
            None => bail!("no current context is set"),
        },

        ConfigCmd::UseContext { name } => {
            if !config.contexts.contains_key(name) {
                bail!("no context named {name:?} — `serverctl config get-contexts` lists them");
            }
            config.current_context = Some(name.clone());
            config.save(&path)?;
            println!("Switched to context {name:?}.");
        }

        ConfigCmd::SetContext(args) => {
            let entry = config.contexts.entry(args.name.clone()).or_default();
            if let Some(s) = &args.server {
                entry.server = s.clone();
            }
            if let Some(u) = &args.user {
                entry.user = Some(u.clone());
            }
            if let Some(p) = &args.password {
                entry.password = Some(p.clone());
                entry.password_command = None;
            }
            if let Some(c) = &args.password_command {
                entry.password_command = Some(c.clone());
                entry.password = None;
            }
            if args.insecure_skip_tls_verify {
                entry.insecure_skip_tls_verify = true;
            }
            if entry.server.is_empty() {
                bail!("context {:?} needs a --server", args.name);
            }
            if args.current {
                config.current_context = Some(args.name.clone());
            }
            config.save(&path)?;
            println!("Context {:?} saved.", args.name);
        }

        ConfigCmd::DeleteContext { name } => {
            if config.contexts.remove(name).is_none() {
                bail!("no context named {name:?}");
            }
            if config.current_context.as_deref() == Some(name.as_str()) {
                config.current_context = config.contexts.keys().next().cloned();
            }
            config.save(&path)?;
            println!("Removed context {name:?}.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::context_name_for;

    #[test]
    fn context_names_come_from_the_host() {
        assert_eq!(context_name_for("http://127.0.0.1:9090"), "local");
        assert_eq!(context_name_for("http://localhost:9090"), "local");
        assert_eq!(context_name_for("https://lb.example.com"), "lb.example.com");
        assert_eq!(context_name_for("https://lb.example.com:443/"), "lb.example.com");
    }
}
