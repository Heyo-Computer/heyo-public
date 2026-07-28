//! serverctl — a kubectl-shaped CLI for the app-lb admin API.
//!
//! The verbs mirror kubectl because the mental model is the same: declarative
//! specs you `apply`, imperative helpers (`create`, `scale`, `set`) that write
//! those specs for you, and read commands (`get`, `describe`, `top`) that render
//! them back. What it drives is app-lb's admin API — deployments, their microVM
//! pools, and the certificates app-lb issues for their hostnames.
//!
//! Two shapes of deployment exist and the difference surfaces everywhere: a
//! *managed* one owns an autoscaled pool of Firecracker/KVM VMs, while a
//! *static* one proxy_passes to fixed upstreams and has neither a scaling policy
//! nor VMs to evict.

mod client;
mod cmd;
mod config;
mod output;
mod spec;
mod types;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use cmd::Ctx;
use output::OutputFormat;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "serverctl",
    version,
    about = "Control an app-lb load balancer: deployments, scaling, VM pools and certificates",
    long_about = "serverctl drives app-lb's admin API.\n\n\
                  By default it talks to http://127.0.0.1:9090 — app-lb's admin listener binds \
                  loopback and speaks plaintext HTTP, so reach a remote one over an SSH tunnel \
                  (ssh -L 9090:127.0.0.1:9090 host) or through app-lb's own TLS listener, and \
                  save it with `serverctl login`.",
    propagate_version = true,
    max_term_width = 100
)]
struct Cli {
    #[command(flatten)]
    globals: GlobalOpts,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
pub struct GlobalOpts {
    /// Output format.
    #[arg(
        long,
        short = 'o',
        global = true,
        value_enum,
        default_value_t = OutputFormat::Table,
        value_name = "FORMAT",
        help_heading = "Global options"
    )]
    pub output: OutputFormat,

    /// Config file holding the saved contexts.
    #[arg(long, global = true, env = "SERVERCTL_CONFIG", value_name = "PATH", help_heading = "Global options")]
    pub config: Option<PathBuf>,

    /// Which stored context to use.
    #[arg(long, global = true, env = "SERVERCTL_CONTEXT", value_name = "NAME", help_heading = "Global options")]
    pub context: Option<String>,

    /// Admin API URL, overriding the context.
    #[arg(long, global = true, env = "SERVERCTL_SERVER", value_name = "URL", help_heading = "Global options")]
    pub server: Option<String>,

    /// Basic-auth user, overriding the context.
    #[arg(long, global = true, env = "SERVERCTL_USER", value_name = "NAME", help_heading = "Global options")]
    pub user: Option<String>,

    /// Basic-auth password, overriding the context. Prefer SERVERCTL_PASSWORD
    /// or `serverctl login` — an argument is visible in `ps`.
    #[arg(
        long,
        global = true,
        env = "SERVERCTL_PASSWORD",
        value_name = "PASSWORD",
        hide_env_values = true,
        help_heading = "Global options"
    )]
    pub password: Option<String>,

    /// Accept any TLS certificate from the server.
    #[arg(long, global = true, help_heading = "Global options")]
    pub insecure_skip_tls_verify: bool,

    /// Per-request timeout.
    #[arg(long, global = true, value_name = "SECS", default_value_t = 30, help_heading = "Global options")]
    pub request_timeout: u64,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Verify credentials against a server and save them as a context.
    Login(cmd::auth::LoginArgs),

    /// Forget a stored context, or just its password.
    Logout(cmd::auth::LogoutArgs),

    /// Show which server and identity the next command would use, and what it
    /// is allowed to do.
    Whoami,

    /// Manage stored contexts.
    Config {
        #[command(subcommand)]
        cmd: cmd::auth::ConfigCmd,
    },

    /// List deployments, VMs or certificates.
    #[command(visible_alias = "list")]
    Get(cmd::read::GetArgs),

    /// Show everything about a deployment: spec, pool, backends and traffic.
    Describe(cmd::read::DescribeArgs),

    /// Register a new deployment from flags.
    Create {
        #[command(subcommand)]
        cmd: CreateCmd,
    },

    /// Create or replace deployments from a spec file (JSON or YAML).
    Apply(cmd::write::ApplyArgs),

    /// Fetch a deployment's spec, open it in $EDITOR, and put it back.
    Edit(cmd::write::EditArgs),

    /// Change one part of a deployment in place.
    Set {
        #[command(subcommand)]
        cmd: SetCmd,
    },

    /// Change a deployment's scaling policy.
    #[command(visible_alias = "autoscale")]
    Scale(cmd::write::ScaleArgs),

    /// Recycle a deployment's VMs, one eviction at a time.
    Restart(cmd::write::RestartArgs),

    /// Watch a pool converge on its desired size.
    Rollout {
        #[command(subcommand)]
        cmd: RolloutCmd,
    },

    /// Deregister a deployment, or evict a VM from one.
    #[command(visible_alias = "rm")]
    Delete(cmd::write::DeleteArgs),

    /// Resource usage for deployments, VMs or the host.
    Top(cmd::observe::TopArgs),

    /// A whole-LB overview: uptime, host, fleet and traffic.
    #[command(visible_alias = "cluster-info")]
    Status,

    /// Print a shell completion script.
    Completion {
        /// bash, zsh, fish, elvish or powershell.
        #[arg(value_name = "SHELL")]
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
enum CreateCmd {
    /// Register a deployment: a managed VM pool, or a static proxy_pass target.
    #[command(visible_aliases = ["deploy", "dep"])]
    Deployment(Box<cmd::write::CreateDeploymentArgs>),
}

#[derive(Subcommand, Debug)]
enum SetCmd {
    /// Point a managed deployment at a different guest image. The pool is
    /// rebuilt, because the running VMs were made from the old one.
    Image(cmd::write::SetImageArgs),

    /// Set or remove guest environment variables (`KEY=VALUE`, or `KEY-`).
    /// Rebuilds the pool for the same reason.
    Env(cmd::write::SetEnvArgs),

    /// Replace a static deployment's upstream addresses.
    Upstreams(cmd::write::SetUpstreamsArgs),

    /// Replace (or, with --add, extend) a deployment's route rules.
    #[command(visible_alias = "routes")]
    Route(cmd::write::SetRouteArgs),
}

#[derive(Subcommand, Debug)]
enum RolloutCmd {
    /// Wait until a deployment's pool is at its desired size and healthy.
    Status(cmd::write::RolloutStatusArgs),

    /// Recycle a deployment's VMs (the same thing as `serverctl restart`).
    Restart(cmd::write::RestartArgs),
}

fn main() {
    // Rust ignores SIGPIPE, which turns `serverctl get deployments | head` into
    // a panic on the first write past the closed pipe. Restore the default
    // disposition so the process just ends, like every other CLI in a pipeline.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<()> {
    let g = &cli.globals;
    match &cli.command {
        // These touch the config file and build at most their own client, so
        // they must keep working against an unreachable server.
        Command::Login(args) => cmd::auth::login(g, args),
        Command::Logout(args) => cmd::auth::logout(g, args),
        Command::Config { cmd } => cmd::auth::config(g, cmd),
        Command::Whoami => cmd::auth::whoami(g),
        Command::Completion { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(*shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }

        Command::Get(args) => cmd::read::get(&Ctx::new(g)?, args),
        Command::Describe(args) => cmd::read::describe(&Ctx::new(g)?, args),
        Command::Create { cmd } => match cmd {
            CreateCmd::Deployment(args) => cmd::write::create(&Ctx::new(g)?, args),
        },
        Command::Apply(args) => cmd::write::apply(&Ctx::new(g)?, args),
        Command::Edit(args) => cmd::write::edit(&Ctx::new(g)?, args),
        Command::Set { cmd } => {
            let ctx = Ctx::new(g)?;
            match cmd {
                SetCmd::Image(args) => cmd::write::set_image(&ctx, args),
                SetCmd::Env(args) => cmd::write::set_env(&ctx, args),
                SetCmd::Upstreams(args) => cmd::write::set_upstreams(&ctx, args),
                SetCmd::Route(args) => cmd::write::set_route(&ctx, args),
            }
        }
        Command::Scale(args) => cmd::write::scale(&Ctx::new(g)?, args),
        Command::Restart(args) => cmd::write::restart(&Ctx::new(g)?, args),
        Command::Rollout { cmd } => {
            let ctx = Ctx::new(g)?;
            match cmd {
                RolloutCmd::Status(args) => cmd::write::rollout_status(&ctx, args),
                RolloutCmd::Restart(args) => cmd::write::restart(&ctx, args),
            }
        }
        Command::Delete(args) => cmd::write::delete(&Ctx::new(g)?, args),
        Command::Top(args) => cmd::observe::top(&Ctx::new(g)?, args),
        Command::Status => cmd::observe::status(&Ctx::new(g)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_definition_is_well_formed() {
        // Catches duplicated flags, bad aliases and broken `requires`/
        // `conflicts_with` references, which clap only validates at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "serverctl",
            "get",
            "deployments",
            "-o",
            "json",
            "--server",
            "http://lb:9090",
        ])
        .unwrap();
        assert_eq!(cli.globals.output, OutputFormat::Json);
        assert_eq!(cli.globals.server.as_deref(), Some("http://lb:9090"));
    }

    #[test]
    fn scale_rejects_replicas_together_with_a_band() {
        assert!(
            Cli::try_parse_from(["serverctl", "scale", "web", "--replicas", "2", "--min", "1"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["serverctl", "scale", "web", "--replicas", "2"]).is_ok());
    }

    #[test]
    fn create_takes_the_shorthand_and_long_route_forms() {
        let cli = Cli::try_parse_from([
            "serverctl",
            "create",
            "deployment",
            "web",
            "--host",
            "web.local",
            "--port",
            "8080",
            "--route",
            "path=/api",
            "-e",
            "RUST_LOG=info",
        ])
        .unwrap();
        let Command::Create {
            cmd: CreateCmd::Deployment(args),
        } = &cli.command
        else {
            panic!("expected create deployment");
        };
        assert_eq!(args.host.as_deref(), Some("web.local"));
        assert_eq!(args.port, Some(8080));
        assert_eq!(args.routes, vec!["path=/api".to_string()]);
        assert_eq!(args.env, vec!["RUST_LOG=info".to_string()]);
    }
}
