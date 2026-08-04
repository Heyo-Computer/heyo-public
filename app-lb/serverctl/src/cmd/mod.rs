//! Command implementations, and the context they share.

pub mod artifact;
pub mod auth;
pub mod observe;
pub mod read;
pub mod session;
pub mod token;
pub mod write;

use crate::blocking::Client;
use clap::Args;
use std::path::PathBuf;
use crate::config::{Config, Endpoint, resolve_endpoint};
use crate::output::OutputFormat;
use anyhow::{Result, bail};
use std::time::Duration;

/// Everything a command needs: a client pointed at the resolved endpoint, and
/// the output format.
/// Where to look when nothing says otherwise.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:9090";

/// The flags every subcommand shares.
///
/// Lives here rather than in `main.rs` because `Ctx` is built from it and both
/// are now library code — the binary is a thin shell over this module.
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

pub struct Ctx {
    pub client: Client,
    pub endpoint: Endpoint,
    pub out: OutputFormat,
}

impl Ctx {
    pub fn new(globals: &GlobalOpts) -> Result<Self> {
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
        let client = Client::connect(
            &endpoint.server,
            endpoint.user.as_deref(),
            endpoint.password.as_deref(),
            endpoint.insecure_skip_tls_verify,
            Duration::from_secs(globals.request_timeout),
        )?;
        Ok(Self {
            client,
            endpoint,
            out: globals.output,
        })
    }
}

/// The resource kinds serverctl addresses, with kubectl-style aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Deployment,
    Vm,
    Cert,
    Secret,
    /// A CI workflow object: which repository the `ci` orchestrator builds, and
    /// on which heyvm network.
    Workflow,
    /// A deploy job: an image build, an artifact pull, or a host update.
    Job,
    /// `get all` — every kind that has a listing.
    All,
}

impl Resource {
    pub fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().trim_end_matches('s') {
            "deployment" | "deploy" | "dep" | "d" | "app" => Some(Self::Deployment),
            "vm" | "instance" | "backend" | "replica" => Some(Self::Vm),
            "cert" | "certificate" | "tl" => Some(Self::Cert),
            "secret" | "sec" => Some(Self::Secret),
            "workflow" | "wf" | "flow" | "ci" => Some(Self::Workflow),
            // `build`, `pull` and `update` name the three verbs; all list the
            // same jobs, so `get builds` and `get pulls` are the same listing
            // filtered by eye. One resource kind, several spellings people will
            // reach for.
            "job" | "build" | "bld" | "pull" | "update" | "run" => Some(Self::Job),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn singular(self) -> &'static str {
        match self {
            Self::Deployment => "deployment",
            Self::Vm => "vm",
            Self::Cert => "cert",
            Self::Secret => "secret",
            Self::Workflow => "workflow",
            Self::Job => "job",
            Self::All => "all",
        }
    }
}

/// Parse kubectl-shaped positional arguments: `deployments`, `deployment web`,
/// `deployment/web`, `deploy web api`, or — when `default_kind` is set — a bare
/// `web`.
pub fn parse_ref(args: &[String], default_kind: Option<Resource>) -> Result<(Resource, Vec<String>)> {
    let Some(first) = args.first() else {
        bail!("no resource given");
    };

    let (kind, mut names): (Resource, Vec<String>) = if let Some((k, name)) = first.split_once('/') {
        let kind = Resource::parse(k)
            .ok_or_else(|| anyhow::anyhow!("unknown resource type {k:?} in {first:?}"))?;
        (kind, vec![name.to_string()])
    } else if let Some(kind) = Resource::parse(first) {
        (kind, Vec::new())
    } else if let Some(kind) = default_kind {
        (kind, vec![first.clone()])
    } else {
        bail!(
            "unknown resource type {first:?} — expected deployments, vms, certs, secrets or jobs"
        );
    };

    // Remaining arguments are names, and may themselves be `type/name`.
    for arg in &args[1..] {
        match arg.split_once('/') {
            Some((k, name)) => {
                let other = Resource::parse(k)
                    .ok_or_else(|| anyhow::anyhow!("unknown resource type {k:?} in {arg:?}"))?;
                if other != kind {
                    bail!("cannot mix {} and {} in one command", kind.singular(), other.singular());
                }
                names.push(name.to_string());
            }
            None => names.push(arg.clone()),
        }
    }
    Ok((kind, names))
}

/// A single `TYPE/NAME` or bare-name argument, for the commands that act on
/// exactly one deployment.
pub fn deployment_name(arg: &str) -> Result<String> {
    let (kind, names) = parse_ref(std::slice::from_ref(&arg.to_string()), Some(Resource::Deployment))?;
    if kind != Resource::Deployment {
        bail!("expected a deployment, got {}", kind.singular());
    }
    match names.len() {
        1 => Ok(names.into_iter().next().expect("len == 1")),
        _ => bail!("expected a deployment name, e.g. `web` or `deployment/web`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plural_and_singular_and_slash_forms_all_work() {
        assert_eq!(parse_ref(&args(&["deployments"]), None).unwrap().0, Resource::Deployment);
        assert_eq!(parse_ref(&args(&["deploy"]), None).unwrap().0, Resource::Deployment);
        let (kind, names) = parse_ref(&args(&["deployment/web"]), None).unwrap();
        assert_eq!((kind, names), (Resource::Deployment, vec!["web".to_string()]));
        let (_, names) = parse_ref(&args(&["deployment", "web", "api"]), None).unwrap();
        assert_eq!(names, vec!["web".to_string(), "api".to_string()]);
    }

    #[test]
    fn a_bare_name_needs_a_default_kind() {
        assert!(parse_ref(&args(&["web"]), None).is_err());
        let (kind, names) = parse_ref(&args(&["web"]), Some(Resource::Deployment)).unwrap();
        assert_eq!((kind, names), (Resource::Deployment, vec!["web".to_string()]));
    }

    #[test]
    fn mixing_kinds_is_refused() {
        assert!(parse_ref(&args(&["deployment/web", "vm/abc"]), None).is_err());
    }

    #[test]
    fn deployment_name_accepts_both_spellings() {
        assert_eq!(deployment_name("web").unwrap(), "web");
        assert_eq!(deployment_name("deployment/web").unwrap(), "web");
        assert_eq!(deployment_name("deploy/web").unwrap(), "web");
        assert!(deployment_name("vm/web").is_err());
    }
}
