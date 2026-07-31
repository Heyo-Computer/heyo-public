//! The commands that change something: create, apply, edit, set, scale,
//! restart and delete.
//!
//! Every in-place change is a read-modify-write against `PUT /deployments/:id`,
//! which replaces the whole spec. The one exception is `scale`, which has a
//! real partial endpoint (`PATCH .../scaling`) and so never has to read first.

use super::{Ctx, Resource, deployment_name, parse_ref};
use crate::output::{self, Table};
use crate::spec::{self, EnvChange};
use crate::types::{DeploymentStatus, JobRecord, SecretSummary};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// -- create ----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct CreateDeploymentArgs {
    /// The deployment id, unique across the load balancer.
    #[arg(value_name = "NAME")]
    pub name: String,

    // Routing. The three shorthand flags describe one rule together; --route
    // adds further rules.
    /// Exact hostname to route, e.g. `secrets.local`.
    #[arg(long, value_name = "HOST", help_heading = "Routing")]
    pub host: Option<String>,
    /// Subdomain match: the apex and any subdomain of it, e.g. `apps.example.com`.
    #[arg(long, value_name = "DOMAIN", help_heading = "Routing")]
    pub host_suffix: Option<String>,
    /// Path prefix to route, e.g. `/api`. Forwarded unchanged — app-lb does not
    /// strip it.
    #[arg(long, value_name = "PATH", help_heading = "Routing")]
    pub path_prefix: Option<String>,
    /// An extra route rule: `host=a.example.com,path=/api`, `*.example.com`, or
    /// `/api`. Repeatable.
    #[arg(long = "route", value_name = "RULE", help_heading = "Routing")]
    pub routes: Vec<String>,
    /// Create with no ingress at all. The deployment takes no HTTP traffic and
    /// is reached only by `serverctl exec` and `serverctl shell` — the usual
    /// shape for an agent sandbox. Managed (VM) deployments only; add a route
    /// later with `serverctl set routes` to expose it.
    #[arg(
        long,
        conflicts_with_all = ["host", "host_suffix", "path_prefix", "routes"],
        help_heading = "Routing"
    )]
    pub no_route: bool,

    // Managed VM pool.
    /// Guest image for the VM pool (defaults to ubuntu:24.04 daemon-side).
    #[arg(long, value_name = "IMAGE", help_heading = "VM pool")]
    pub image: Option<String>,
    /// Guest port traffic is proxied to. Required for a managed deployment.
    #[arg(long, value_name = "PORT", help_heading = "VM pool")]
    pub port: Option<u16>,
    /// Hypervisor driver. libvirt is rejected: app-lb routes to the guest IP,
    /// which only tap-networked firecracker/kvm expose.
    #[arg(long, value_name = "DRIVER", default_value = "firecracker", help_heading = "VM pool")]
    pub driver: String,
    /// Command the guest runs at boot.
    #[arg(long, value_name = "CMD", help_heading = "VM pool")]
    pub start_command: Option<String>,
    /// Size class: micro, mini, small, medium, large, xlarge.
    #[arg(long, value_name = "CLASS", help_heading = "VM pool")]
    pub size: Option<String>,
    /// Size of the guest's persistent data disk, mounted at /workspace. This is
    /// the *only* storage that survives a stop — the root filesystem is recopied
    /// from the image on every boot — so a sandbox that keeps state needs it.
    #[arg(long, value_name = "GB", help_heading = "VM pool")]
    pub disk_gb: Option<u32>,
    #[arg(long, value_name = "DIR", help_heading = "VM pool")]
    pub workdir: Option<String>,
    /// Guest environment variable, `KEY=VALUE`. Repeatable.
    #[arg(long = "env", short = 'e', value_name = "KEY=VALUE", help_heading = "VM pool")]
    pub env: Vec<String>,
    /// Command run once while the VM is being prepared. Repeatable.
    #[arg(long = "setup-hook", value_name = "CMD", help_heading = "VM pool")]
    pub setup_hooks: Vec<String>,
    /// Extra guest port to open. Repeatable.
    #[arg(long = "open-port", value_name = "PORT", help_heading = "VM pool")]
    pub open_ports: Vec<u16>,
    /// Backstop TTL in seconds: VMs die on their own if app-lb stops renewing.
    #[arg(long, value_name = "SECS", help_heading = "VM pool")]
    pub ttl: Option<u64>,

    /// A fixed upstream `host:port` to proxy_pass to, instead of a VM pool.
    /// Repeatable; mutually exclusive with the VM-pool flags.
    #[arg(long = "upstream", value_name = "ADDR", help_heading = "Static upstreams")]
    pub upstreams: Vec<String>,

    // Build source. Recording it here does not build anything — `serverctl
    // build <name>` does that — so a deployment can be created against an
    // existing image and switched to built images later.
    /// Git remote the guest image is built from.
    #[arg(long, value_name = "URL", help_heading = "Build source")]
    pub repo: Option<String>,
    /// Branch, tag or commit to build. Unset follows the remote's default branch.
    #[arg(long = "ref", value_name = "REF", help_heading = "Build source", requires = "repo")]
    pub git_ref: Option<String>,
    /// Dockerfile path within the repo. Unset lets app-lb look for one.
    #[arg(long, value_name = "PATH", help_heading = "Build source", requires = "repo")]
    pub dockerfile: Option<String>,
    /// Build context within the repo. Defaults to the Dockerfile's directory.
    #[arg(long = "build-context", value_name = "PATH", help_heading = "Build source", requires = "repo")]
    pub build_context: Option<String>,
    /// Base name for built images; the commit is appended.
    #[arg(long, value_name = "NAME", help_heading = "Build source", requires = "repo")]
    pub image_name: Option<String>,
    /// Rootfs size for the built image.
    #[arg(long = "size-mb", value_name = "MB", help_heading = "Build source", requires = "repo")]
    pub image_size_mb: Option<u64>,
    /// Stored secret holding the git credential, as `NAME` or `NAME/KEY`.
    #[arg(long = "secret", value_name = "NAME[/KEY]", help_heading = "Build source", requires = "repo")]
    pub secret: Option<String>,

    #[command(flatten)]
    pub scaling: ScalingFlags,

    /// Health probe path. Defaults to `/`.
    #[arg(long, value_name = "PATH", help_heading = "Health")]
    pub health_path: Option<String>,
    /// Prove readiness with a bare TCP connect instead of an HTTP request.
    #[arg(long, conflicts_with = "health_path", help_heading = "Health")]
    pub health_tcp: bool,
    /// Health port, if the guest serves health somewhere other than --port.
    #[arg(long, value_name = "PORT", help_heading = "Health")]
    pub health_port: Option<u16>,
    #[arg(long, value_name = "SECS", help_heading = "Health")]
    pub health_timeout: Option<u64>,

    /// Print the spec that would be sent, and send nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// The scaling knobs, shared by `create` and `scale`.
#[derive(Args, Debug, Default)]
pub struct ScalingFlags {
    /// Floor for the pool: replicas kept even at zero traffic.
    #[arg(long, value_name = "N", help_heading = "Scaling")]
    pub min: Option<u64>,
    /// Ceiling for the pool.
    #[arg(long, value_name = "N", help_heading = "Scaling")]
    pub max: Option<u64>,
    /// Idle-but-ready spares kept above what load requires.
    #[arg(long, value_name = "N", help_heading = "Scaling")]
    pub warm: Option<u64>,
    /// In-flight requests per VM the autoscaler aims for.
    #[arg(long, value_name = "N", help_heading = "Scaling")]
    pub target_concurrency: Option<u64>,
    /// Idle time before the pool drops to min_replicas.
    #[arg(long, value_name = "SECS", help_heading = "Scaling")]
    pub scale_to_zero_after: Option<u64>,
    /// How long a request waits for a VM to boot before giving up with 503.
    #[arg(long, value_name = "SECS", help_heading = "Scaling")]
    pub cold_start_timeout: Option<u64>,
    /// How long a draining VM may keep serving before it is killed anyway.
    #[arg(long, value_name = "SECS", help_heading = "Scaling")]
    pub drain_timeout: Option<u64>,
    /// How long a booting VM has to pass its health check before the autoscaler
    /// gives up on it and replaces it. 0 waits indefinitely.
    #[arg(long, value_name = "SECS", help_heading = "Scaling")]
    pub boot_timeout: Option<u64>,
    /// What becomes of a VM the autoscaler retires. `destroy` (the default)
    /// frees the sandbox and its disks. `retain` stops it instead, keeping its
    /// /workspace data disk, and a later request or `exec` resumes that VM
    /// rather than booting a fresh one — the setting for an agent sandbox,
    /// whose working directory is the point.
    #[arg(
        long,
        value_name = "ACTION",
        value_parser = ["destroy", "retain"],
        help_heading = "Scaling"
    )]
    pub idle_action: Option<String>,
}

impl ScalingFlags {
    fn patch(&self) -> Map<String, Value> {
        let mut patch = spec::scaling_patch(&[
            ("min_replicas", self.min),
            ("max_replicas", self.max),
            ("warm_pool", self.warm),
            ("target_concurrency", self.target_concurrency),
            ("scale_to_zero_after_secs", self.scale_to_zero_after),
            ("cold_start_timeout_secs", self.cold_start_timeout),
            ("drain_timeout_secs", self.drain_timeout),
            ("boot_timeout_secs", self.boot_timeout),
        ]);
        if let Some(action) = &self.idle_action {
            patch.insert("idle_action".into(), Value::String(action.clone()));
        }
        patch
    }
}

pub fn create(ctx: &Ctx, args: &CreateDeploymentArgs) -> Result<()> {
    let spec = build_spec(args)?;
    if args.dry_run {
        return print_spec(ctx, &spec);
    }
    let created = ctx.client.create_deployment(&spec)?;
    report_write(ctx, &created, &args.name, "created")?;
    // Recording a build source does not build anything; say so, because the
    // deployment is otherwise sitting on whatever --image named.
    if args.repo.is_some() && !ctx.out.is_machine() {
        println!(
            "\nIts image is not built yet — run `serverctl build {}` to check out the repo, \
             build the Dockerfile and roll the pool onto the result.",
            args.name
        );
    }
    Ok(())
}

fn build_spec(args: &CreateDeploymentArgs) -> Result<Value> {
    let mut routes = Vec::new();
    if let Some(rule) = spec::route_from_parts(
        args.host.as_deref(),
        args.host_suffix.as_deref(),
        args.path_prefix.as_deref(),
    ) {
        routes.push(rule);
    }
    for r in &args.routes {
        routes.push(spec::parse_route(r)?);
    }
    if routes.is_empty() && !args.no_route {
        bail!(
            "a deployment needs at least one route — pass --host, --host-suffix, \
             --path-prefix or --route, or --no-route for a sandbox reached only \
             by exec/shell"
        );
    }
    if args.no_route && !args.upstreams.is_empty() {
        bail!(
            "--no-route leaves nothing able to reach this deployment: a static \
             (proxy_pass) deployment has no exec/shell door, so the proxy is its \
             only way in"
        );
    }

    let mut spec = Map::new();
    spec.insert("id".into(), Value::String(args.name.clone()));
    spec.insert("routes".into(), Value::Array(routes));

    let vm_flags_used = args.image.is_some()
        || args.port.is_some()
        || args.start_command.is_some()
        || args.size.is_some()
        || !args.env.is_empty();

    if !args.upstreams.is_empty() {
        if vm_flags_used {
            bail!(
                "--upstream makes this a static (proxy_pass) deployment, which has no VM \
                 template — drop the VM flags, or drop --upstream"
            );
        }
        spec.insert(
            "upstreams".into(),
            Value::Array(args.upstreams.iter().cloned().map(Value::String).collect()),
        );
    } else {
        let port = args.port.context(
            "a managed deployment needs --port (the guest port to proxy to); \
             use --upstream instead for a static proxy_pass deployment",
        )?;
        let mut vm = Map::new();
        vm.insert("driver".into(), Value::String(args.driver.to_ascii_lowercase()));
        vm.insert("port".into(), Value::from(port));
        insert_opt_str(&mut vm, "image", args.image.as_deref());
        insert_opt_str(&mut vm, "start_command", args.start_command.as_deref());
        insert_opt_str(&mut vm, "size_class", args.size.as_deref().map(str::to_ascii_lowercase).as_deref());
        insert_opt_str(&mut vm, "working_directory", args.workdir.as_deref());
        if let Some(gb) = args.disk_gb {
            vm.insert("disk_size_gb".into(), Value::from(gb));
        }
        if let Some(ttl) = args.ttl {
            vm.insert("ttl_seconds".into(), Value::from(ttl));
        }
        if !args.env.is_empty() {
            let mut env = Map::new();
            for e in &args.env {
                match spec::parse_env(e)? {
                    EnvChange::Set(k, v) => {
                        env.insert(k, Value::String(v));
                    }
                    EnvChange::Remove(k) => {
                        bail!("--env {k}- removes a variable; there is nothing to remove on create")
                    }
                }
            }
            vm.insert("env_vars".into(), Value::Object(env));
        }
        if !args.setup_hooks.is_empty() {
            vm.insert(
                "setup_hooks".into(),
                Value::Array(args.setup_hooks.iter().cloned().map(Value::String).collect()),
            );
        }
        if !args.open_ports.is_empty() {
            vm.insert(
                "open_ports".into(),
                Value::Array(args.open_ports.iter().map(|p| Value::from(*p)).collect()),
            );
        }
        spec.insert("vm".into(), Value::Object(vm));
    }

    if let Some(repo) = &args.repo {
        if !args.upstreams.is_empty() {
            bail!(
                "--repo builds a guest image, which a static (proxy_pass) deployment does \
                 not have — drop --upstream, or drop the build flags"
            );
        }
        let mut build = Map::new();
        build.insert("repo".into(), Value::String(repo.clone()));
        insert_opt_str(&mut build, "ref", args.git_ref.as_deref());
        insert_opt_str(&mut build, "dockerfile", args.dockerfile.as_deref());
        insert_opt_str(&mut build, "context", args.build_context.as_deref());
        insert_opt_str(&mut build, "image_name", args.image_name.as_deref());
        if let Some(mb) = args.image_size_mb {
            build.insert("image_size_mb".into(), Value::from(mb));
        }
        if let Some(s) = &args.secret {
            build.insert("auth".into(), spec::parse_secret_ref(s)?);
        }
        spec.insert("build".into(), Value::Object(build));
    }

    let scaling = args.scaling.patch();
    if !scaling.is_empty() {
        if !args.upstreams.is_empty() {
            bail!("a static (proxy_pass) deployment is not autoscaled, so the scaling flags do nothing");
        }
        spec.insert("scaling".into(), Value::Object(scaling));
    }

    let mut health = Map::new();
    if args.health_tcp {
        // Explicit null is what selects a bare TCP connect; omitting the field
        // would get the server's default of `GET /`.
        health.insert("path".into(), Value::Null);
    } else if let Some(p) = &args.health_path {
        health.insert("path".into(), Value::String(p.clone()));
    }
    if let Some(port) = args.health_port {
        health.insert("port".into(), Value::from(port));
    }
    if let Some(t) = args.health_timeout {
        health.insert("timeout_secs".into(), Value::from(t));
    }
    if !health.is_empty() {
        spec.insert("health".into(), Value::Object(health));
    }

    Ok(Value::Object(spec))
}

fn insert_opt_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        map.insert(key.to_string(), Value::String(v.to_string()));
    }
}

// -- create secret ---------------------------------------------------------

/// Where a secret's values come from.
///
/// Four sources rather than one because a credential on the command line is
/// visible in `ps` and lands in shell history — that is the convenient form, so
/// it stays, but the alternatives have to be just as easy to reach.
#[derive(Args, Debug, Default)]
pub struct SecretSourceFlags {
    /// Read the value from a file: `KEY=/path/to/token`. A single trailing
    /// newline is stripped, which is what `openssl rand … > file` leaves.
    #[arg(long = "from-file", value_name = "KEY=PATH", help_heading = "Sources")]
    pub from_file: Vec<String>,
    /// Take the value from this process's environment: `KEY=VAR`, or just `KEY`
    /// to use the key's own name as the variable.
    #[arg(long = "from-env", value_name = "KEY[=VAR]", help_heading = "Sources")]
    pub from_env: Vec<String>,
    /// Read one value from stdin: `--from-stdin KEY`. Everything up to EOF is
    /// the value, minus a trailing newline.
    #[arg(long = "from-stdin", value_name = "KEY", help_heading = "Sources")]
    pub from_stdin: Option<String>,
}

impl SecretSourceFlags {
    /// Collect every source into `KEY -> value`, in flag order.
    fn collect(&self, literals: &[String]) -> Result<Map<String, Value>> {
        let mut data = Map::new();
        for arg in literals {
            match spec::parse_env(arg)? {
                EnvChange::Set(k, v) => {
                    data.insert(k, Value::String(v));
                }
                EnvChange::Remove(k) => bail!(
                    "{k}- removes a key; there is nothing to remove while creating a secret"
                ),
            }
        }
        for arg in &self.from_file {
            let (key, path) = arg.split_once('=').with_context(|| {
                format!("--from-file {arg:?} is not KEY=PATH")
            })?;
            let value = std::fs::read_to_string(path)
                .with_context(|| format!("reading the value for {key:?} from {path}"))?;
            data.insert(key.to_string(), Value::String(trim_one_newline(&value)));
        }
        for arg in &self.from_env {
            let (key, var) = match arg.split_once('=') {
                Some((k, v)) => (k, v),
                None => (arg.as_str(), arg.as_str()),
            };
            let value = std::env::var(var).with_context(|| {
                format!("${var} is not set, so there is no value for {key:?}")
            })?;
            data.insert(key.to_string(), Value::String(value));
        }
        if let Some(key) = &self.from_stdin {
            let value = std::io::read_to_string(std::io::stdin())
                .with_context(|| format!("reading the value for {key:?} from stdin"))?;
            data.insert(key.clone(), Value::String(trim_one_newline(&value)));
        }
        Ok(data)
    }
}

fn trim_one_newline(s: &str) -> String {
    s.strip_suffix('\n')
        .map(|t| t.strip_suffix('\r').unwrap_or(t))
        .unwrap_or(s)
        .to_string()
}

#[derive(Args, Debug)]
pub struct CreateSecretArgs {
    /// The secret id, unique across the load balancer.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// `KEY=VALUE`, repeatable. Visible in `ps` and in shell history — prefer
    /// --from-file, --from-env or --from-stdin for anything real.
    #[arg(value_name = "KEY=VALUE")]
    pub literals: Vec<String>,

    #[command(flatten)]
    pub sources: SecretSourceFlags,

    /// What this secret is for. Shown by `serverctl get secrets`.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Print what would be sent — with the values redacted — and send nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn create_secret(ctx: &Ctx, args: &CreateSecretArgs) -> Result<()> {
    let data = args.sources.collect(&args.literals)?;
    if data.is_empty() {
        bail!(
            "a secret needs at least one key — pass KEY=VALUE, --from-file, --from-env \
             or --from-stdin"
        );
    }

    let mut body = Map::new();
    body.insert("id".into(), Value::String(args.name.clone()));
    if let Some(d) = &args.description {
        body.insert("description".into(), Value::String(d.clone()));
    }
    body.insert("data".into(), Value::Object(data.clone()));

    if args.dry_run {
        // Never print the values, not even here: a dry run is the command people
        // paste into a terminal that somebody else is watching.
        let mut shown = body.clone();
        shown.insert(
            "data".into(),
            Value::Object(
                data.keys()
                    .map(|k| (k.clone(), Value::String("<redacted>".into())))
                    .collect(),
            ),
        );
        return print_spec(ctx, &Value::Object(shown));
    }

    // POST upserts, so say which one actually happened.
    let existed = ctx.client.secret_exists(&args.name).unwrap_or(false);
    let result = ctx.client.create_secret(&Value::Object(body))?;
    report_secret(ctx, &result, &args.name, if existed { "replaced" } else { "created" })
}

#[derive(Args, Debug)]
pub struct SetSecretArgs {
    /// The secret, e.g. `github` or `secret/github`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// `KEY=VALUE` to set, `KEY-` to remove. Repeatable. Keys not mentioned are
    /// left as they are — which matters here, because there is no way to read
    /// them back and resend them.
    #[arg(value_name = "KEY=VALUE")]
    pub changes: Vec<String>,

    #[command(flatten)]
    pub sources: SecretSourceFlags,

    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_secret(ctx: &Ctx, args: &SetSecretArgs) -> Result<()> {
    let id = secret_name(&args.resource)?;

    // Sets and removals share one map: `null` is how the API spells "remove".
    let mut data = Map::new();
    let mut removed = Vec::new();
    for arg in &args.changes {
        match spec::parse_env(arg)? {
            EnvChange::Set(k, v) => {
                data.insert(k, Value::String(v));
            }
            EnvChange::Remove(k) => {
                data.insert(k.clone(), Value::Null);
                removed.push(k);
            }
        }
    }
    for (k, v) in args.sources.collect(&[])? {
        data.insert(k, v);
    }
    if data.is_empty() && args.description.is_none() {
        bail!(
            "nothing to change — pass KEY=VALUE, KEY-, --from-file, --from-env, \
             --from-stdin or --description"
        );
    }

    let mut body = Map::new();
    body.insert("data".into(), Value::Object(data.clone()));
    if let Some(d) = &args.description {
        body.insert("description".into(), Value::String(d.clone()));
    }

    if args.dry_run {
        let redacted: Map<String, Value> = data
            .iter()
            .map(|(k, v)| {
                let shown = if v.is_null() {
                    Value::Null
                } else {
                    Value::String("<redacted>".into())
                };
                (k.clone(), shown)
            })
            .collect();
        let mut shown = body.clone();
        shown.insert("data".into(), Value::Object(redacted));
        return print_spec(ctx, &Value::Object(shown));
    }

    let result = ctx.client.patch_secret(&id, &Value::Object(body))?;
    report_secret(ctx, &result, &id, "updated")
}

/// A single `secret/NAME` or bare-name argument.
fn secret_name(arg: &str) -> Result<String> {
    let (kind, names) = parse_ref(std::slice::from_ref(&arg.to_string()), Some(Resource::Secret))?;
    if kind != Resource::Secret {
        bail!("expected a secret, got {}", kind.singular());
    }
    match names.len() {
        1 => Ok(names.into_iter().next().expect("len == 1")),
        _ => bail!("expected a secret name, e.g. `github` or `secret/github`"),
    }
}

fn report_secret(ctx: &Ctx, result: &Value, id: &str, verb: &str) -> Result<()> {
    if ctx.out.is_machine() {
        return output::emit(result, ctx.out, &[format!("secret/{id}")]);
    }
    match serde_json::from_value::<SecretSummary>(result.clone()) {
        Ok(s) => println!(
            "secret/{id} {verb} — {} key(s): {}{}",
            s.keys.len(),
            s.keys.join(", "),
            if s.encrypted_at_rest {
                " (encrypted at rest)"
            } else {
                " (stored in plaintext; set APP_LB_SECRET_KEY on the server to encrypt)"
            }
        ),
        Err(_) => println!("secret/{id} {verb}"),
    }
    Ok(())
}

// -- set build -------------------------------------------------------------

#[derive(Args, Debug)]
pub struct SetBuildArgs {
    /// The deployment, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Git remote to build from: `https://…`, `git@host:path`, or a path on the
    /// app-lb host. Required the first time.
    #[arg(long, value_name = "URL")]
    pub repo: Option<String>,
    /// Branch, tag or commit to build. Unset follows the remote's default branch.
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// Dockerfile path within the repo. Unset lets app-lb look for one.
    #[arg(long, value_name = "PATH")]
    pub dockerfile: Option<String>,
    /// Build context within the repo. Defaults to the Dockerfile's directory.
    #[arg(long = "build-context", value_name = "PATH")]
    pub build_context: Option<String>,
    /// Base name for built images; the commit is appended. Defaults to the
    /// deployment id.
    #[arg(long, value_name = "NAME")]
    pub image_name: Option<String>,
    /// Rootfs size for the built image. Unset lets heyvm size it from the image
    /// contents.
    #[arg(long = "size-mb", value_name = "MB")]
    pub image_size_mb: Option<u64>,
    /// Credential for a private repo: a stored secret, as `NAME` or `NAME/KEY`
    /// (default key `token`).
    #[arg(long = "secret", value_name = "NAME[/KEY]")]
    pub secret: Option<String>,
    /// Username to pair with the token. Only needed by forges that reject a
    /// placeholder; GitHub, GitLab and Bitbucket do not.
    #[arg(long, value_name = "NAME", requires = "secret")]
    pub username: Option<String>,
    /// Build without credentials (drops `build.auth`).
    #[arg(long, conflicts_with_all = ["secret", "username"])]
    pub no_auth: bool,
    /// Remove the build source entirely; the deployment keeps its current image.
    #[arg(long, conflicts_with_all = [
        "repo", "git_ref", "dockerfile", "build_context", "image_name",
        "image_size_mb", "secret", "username", "no_auth",
    ])]
    pub clear: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_build(ctx: &Ctx, args: &SetBuildArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    if args.clear {
        return edit_spec(ctx, &id, args.dry_run, "build source removed", |spec| {
            if let Some(map) = spec.as_object_mut() {
                map.remove("build");
            }
            Ok(())
        });
    }

    let touched = args.repo.is_some()
        || args.git_ref.is_some()
        || args.dockerfile.is_some()
        || args.build_context.is_some()
        || args.image_name.is_some()
        || args.image_size_mb.is_some()
        || args.secret.is_some()
        || args.no_auth;
    if !touched {
        bail!(
            "nothing to set — pass --repo, --ref, --dockerfile, --context, --image-name, \
             --size-mb, --secret or --no-auth (or --clear to remove the build source)"
        );
    }

    let auth = match &args.secret {
        Some(s) => {
            let mut r = spec::parse_secret_ref(s)?;
            if let (Some(u), Some(map)) = (&args.username, r.as_object_mut()) {
                map.insert("username".into(), Value::String(u.clone()));
            }
            Some(r)
        }
        None => None,
    };

    edit_spec(ctx, &id, args.dry_run, "build source updated", |spec| {
        let build = spec::build_mut(spec, &id)?;
        if build.get("repo").and_then(Value::as_str).is_none() && args.repo.is_none() {
            bail!(
                "deployment {id:?} has no build source yet, so --repo is required \
                 (e.g. --repo https://github.com/acme/web.git)"
            );
        }
        for (key, value) in [
            ("repo", args.repo.as_deref()),
            ("ref", args.git_ref.as_deref()),
            ("dockerfile", args.dockerfile.as_deref()),
            ("context", args.build_context.as_deref()),
            ("image_name", args.image_name.as_deref()),
        ] {
            if let Some(v) = value {
                build.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
        if let Some(mb) = args.image_size_mb {
            build.insert("image_size_mb".into(), Value::from(mb));
        }
        if let Some(auth) = &auth {
            build.insert("auth".into(), auth.clone());
        }
        if args.no_auth {
            build.remove("auth");
        }
        Ok(())
    })
}

// -- set artifact ----------------------------------------------------------

#[derive(Args, Debug)]
pub struct SetArtifactArgs {
    /// The deployment, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// The artifact store: an `art serve` URL (`http://host:8080`) or an
    /// absolute path to a store root on the app-lb host. Required the first time.
    #[arg(long, value_name = "URL|PATH")]
    pub store: Option<String>,
    /// Tag or digest naming the rootfs. A tag follows whatever it is moved to;
    /// a digest is immutable, and is what a rollback should name.
    #[arg(long = "ref", value_name = "REF")]
    pub artifact_ref: Option<String>,
    /// Base name for pulled images; the digest is appended. Defaults to the
    /// deployment id.
    #[arg(long, value_name = "NAME")]
    pub image_name: Option<String>,
    /// Grow the pulled rootfs to this many gigabytes. Sparse, so it costs no
    /// disk until the guest writes to it.
    #[arg(long = "grow-gb", value_name = "GB")]
    pub grow_gb: Option<u64>,
    /// API key for a gated store: a stored secret, as `NAME` or `NAME/KEY`
    /// (default key `token`). Only meaningful for the URL form.
    #[arg(long = "secret", value_name = "NAME[/KEY]")]
    pub secret: Option<String>,
    /// Pull without credentials (drops `artifact.auth`).
    #[arg(long, conflicts_with = "secret")]
    pub no_auth: bool,
    /// Remove the artifact source entirely; the deployment keeps its current
    /// image.
    #[arg(long, conflicts_with_all = [
        "store", "artifact_ref", "image_name", "grow_gb", "secret", "no_auth",
    ])]
    pub clear: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_artifact(ctx: &Ctx, args: &SetArtifactArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    if args.clear {
        return edit_spec(ctx, &id, args.dry_run, "artifact source removed", |spec| {
            if let Some(map) = spec.as_object_mut() {
                map.remove("artifact");
            }
            Ok(())
        });
    }

    let touched = args.store.is_some()
        || args.artifact_ref.is_some()
        || args.image_name.is_some()
        || args.grow_gb.is_some()
        || args.secret.is_some()
        || args.no_auth;
    if !touched {
        bail!(
            "nothing to set — pass --store, --ref, --image-name, --grow-gb, --secret or \
             --no-auth (or --clear to remove the artifact source)"
        );
    }

    let auth = match &args.secret {
        Some(s) => Some(spec::parse_secret_ref(s)?),
        None => None,
    };

    edit_spec(ctx, &id, args.dry_run, "artifact source updated", |spec| {
        let artifact = spec::artifact_mut(spec, &id)?;
        // Both are required to pull at all, and a block with one of them is a
        // spec app-lb would reject on `PUT` — so say which is missing here,
        // where the flag that would fix it is still in view.
        if artifact.get("store").and_then(Value::as_str).is_none() && args.store.is_none() {
            bail!(
                "deployment {id:?} has no artifact source yet, so --store is required \
                 (e.g. --store http://127.0.0.1:8080, or --store /srv/artifacts)"
            );
        }
        if artifact.get("ref").and_then(Value::as_str).is_none() && args.artifact_ref.is_none() {
            bail!(
                "deployment {id:?} has no artifact ref yet, so --ref is required \
                 (a tag like `debian-hermes`, or a digest). \
                 `serverctl artifact ls` lists a store's tags"
            );
        }
        for (key, value) in [
            ("store", args.store.as_deref()),
            ("ref", args.artifact_ref.as_deref()),
            ("image_name", args.image_name.as_deref()),
        ] {
            if let Some(v) = value {
                artifact.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
        if let Some(gb) = args.grow_gb {
            artifact.insert("grow_gb".into(), Value::from(gb));
        }
        if let Some(auth) = &auth {
            artifact.insert("auth".into(), auth.clone());
        }
        if args.no_auth {
            artifact.remove("auth");
        }
        Ok(())
    })
}

// -- pull ------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct PullArgs {
    /// The deployment to pull for, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Pull this reference instead of the one in the spec. A one-off: the
    /// stored `artifact.ref` is left alone, which is what makes
    /// `--ref <digest>` a rollback rather than a config change.
    #[arg(long = "ref", value_name = "REF")]
    pub artifact_ref: Option<String>,

    /// Re-fetch even when the image is already on the app-lb host. Rarely
    /// wanted: the image filename is its digest, so its presence already proves
    /// the bytes are right.
    #[arg(long)]
    pub force: bool,

    /// Wait for the pull to finish and report the outcome.
    #[arg(long, short = 'w')]
    pub wait: bool,

    /// Print the pull's output when it finishes. Implies --wait.
    #[arg(long)]
    pub logs: bool,

    #[arg(long, value_name = "SECS", default_value_t = 1800)]
    pub timeout: u64,
}

pub fn pull(ctx: &Ctx, args: &PullArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    let mut body = Map::new();
    if let Some(r) = &args.artifact_ref {
        body.insert("ref".into(), Value::String(r.clone()));
    }
    if args.force {
        body.insert("force".into(), Value::Bool(true));
    }
    let started = ctx.client.start_pull(&id, &Value::Object(body))?;

    if ctx.out.is_machine() && !(args.wait || args.logs) {
        let record: JobRecord = serde_json::from_value(started.clone()).unwrap_or_default();
        return output::emit(&started, ctx.out, &[format!("job/{}", record.id)]);
    }

    let record: JobRecord =
        serde_json::from_value(started).context("parsing the job the server started")?;
    if !ctx.out.is_machine() {
        println!(
            "job/{} started for deployment/{id} — {} from {}",
            record.id,
            record.artifact_ref.as_deref().unwrap_or("(spec ref)"),
            record.store.as_deref().unwrap_or("(spec store)"),
        );
    }

    if !(args.wait || args.logs) {
        println!(
            "\nIt runs on the app-lb host, and takes as long as the transfer does — or no \
             time at all if the image is already there. Follow it with \
             `serverctl get job {}`.",
            record.id
        );
        return Ok(());
    }
    wait_job(ctx, &record.id, Duration::from_secs(args.timeout), args.logs)
}

// -- set update ------------------------------------------------------------

#[derive(Args, Debug)]
pub struct SetUpdateArgs {
    /// The static deployment, e.g. `app-obs` or `deployment/app-obs`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Working directory on the **app-lb host** — where the commands run.
    /// Required the first time. Must be absolute.
    #[arg(long = "workdir", visible_alias = "working-dir", value_name = "DIR")]
    pub working_dir: Option<String>,

    /// A command to run, in order. Repeatable; each is a shell line, so
    /// `--command 'git pull && cargo build --release'` is one step. Passing any
    /// replaces the whole list.
    #[arg(long = "command", short = 'c', value_name = "CMD")]
    pub commands: Vec<String>,

    /// Environment for the commands, `KEY=VALUE`. Repeatable; replaces the
    /// existing set.
    #[arg(long = "env", short = 'e', value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Environment from a stored secret: `NAME/KEY`, or `ENV=NAME/KEY` to choose
    /// the variable name. Repeatable; replaces the existing set.
    #[arg(long = "secret-env", value_name = "[ENV=]NAME/KEY")]
    pub secret_env: Vec<String>,

    /// Stored secret holding a git credential, for commands that fetch
    /// (`git pull`): `NAME` or `NAME/KEY`.
    #[arg(long = "secret", value_name = "NAME[/KEY]")]
    pub secret: Option<String>,

    /// Run without a git credential (drops `update.auth`).
    #[arg(long, conflicts_with = "secret")]
    pub no_auth: bool,

    /// Ceiling on a single command.
    #[arg(long = "command-timeout", value_name = "SECS")]
    pub timeout_secs: Option<u64>,

    /// How long to wait for the upstreams to answer afterwards. `0` skips the
    /// check — right only when the commands restart nothing.
    #[arg(long = "verify-timeout", value_name = "SECS")]
    pub verify_timeout_secs: Option<u64>,

    /// Remove the update block entirely.
    #[arg(long, conflicts_with_all = [
        "working_dir", "commands", "env", "secret_env", "secret", "no_auth",
        "timeout_secs", "verify_timeout_secs",
    ])]
    pub clear: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_update(ctx: &Ctx, args: &SetUpdateArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    if args.clear {
        return edit_spec(ctx, &id, args.dry_run, "update commands removed", |spec| {
            if let Some(map) = spec.as_object_mut() {
                map.remove("update");
            }
            Ok(())
        });
    }

    let touched = args.working_dir.is_some()
        || !args.commands.is_empty()
        || !args.env.is_empty()
        || !args.secret_env.is_empty()
        || args.secret.is_some()
        || args.no_auth
        || args.timeout_secs.is_some()
        || args.verify_timeout_secs.is_some();
    if !touched {
        bail!(
            "nothing to set — pass --workdir, --command, --env, --secret-env, --secret, \
             --command-timeout or --verify-timeout (or --clear to remove the update block)"
        );
    }

    // Parse everything before the read-modify-write, so a typo fails without
    // having touched the server.
    let auth = match &args.secret {
        Some(s) => Some(spec::parse_secret_ref(s)?),
        None => None,
    };
    let secret_env: Vec<Value> = args
        .secret_env
        .iter()
        .map(|s| spec::parse_secret_env(s))
        .collect::<Result<_>>()?;
    let mut env = Map::new();
    for e in &args.env {
        match spec::parse_env(e)? {
            EnvChange::Set(k, v) => {
                env.insert(k, Value::String(v));
            }
            EnvChange::Remove(k) => bail!(
                "--env {k}- removes a variable, but --env replaces the whole set here; \
                 pass the variables you want to keep"
            ),
        }
    }

    edit_spec(ctx, &id, args.dry_run, "update commands set", |spec| {
        let update = spec::update_mut(spec, &id)?;
        if update.get("working_dir").and_then(Value::as_str).is_none()
            && args.working_dir.is_none()
        {
            bail!(
                "deployment {id:?} has no update block yet, so --workdir is required \
                 (the directory on the app-lb host where the commands run)"
            );
        }
        if update.get("commands").and_then(Value::as_array).is_none_or(|c| c.is_empty())
            && args.commands.is_empty()
        {
            bail!(
                "deployment {id:?} has no update commands yet, so at least one --command is \
                 required (e.g. -c 'git pull --ff-only' -c 'cargo build --release')"
            );
        }

        if let Some(dir) = &args.working_dir {
            update.insert("working_dir".into(), Value::String(dir.clone()));
        }
        if !args.commands.is_empty() {
            update.insert(
                "commands".into(),
                Value::Array(args.commands.iter().cloned().map(Value::String).collect()),
            );
        }
        if !args.env.is_empty() {
            update.insert("env".into(), Value::Object(env.clone()));
        }
        if !secret_env.is_empty() {
            update.insert("env_from".into(), Value::Array(secret_env.clone()));
        }
        if let Some(auth) = &auth {
            update.insert("auth".into(), auth.clone());
        }
        if args.no_auth {
            update.remove("auth");
        }
        if let Some(t) = args.timeout_secs {
            update.insert("timeout_secs".into(), Value::from(t));
        }
        if let Some(t) = args.verify_timeout_secs {
            update.insert("verify_timeout_secs".into(), Value::from(t));
        }
        Ok(())
    })
}

// -- set auth --------------------------------------------------------------

#[derive(Args, Debug)]
pub struct SetAuthArgs {
    /// The deployment to gate, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// OAuth client id from the Google Cloud console. Required the first time.
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,

    /// Stored secret holding the client secret: `NAME` or `NAME/KEY`.
    /// Required the first time.
    #[arg(long = "secret", value_name = "NAME[/KEY]")]
    pub secret: Option<String>,

    /// A Google Workspace domain whose accounts may enter. Repeatable; passing
    /// any replaces the list. `*` means any Google account.
    #[arg(long = "allow-domain", value_name = "DOMAIN")]
    pub allow_domains: Vec<String>,

    /// An individual address allowed regardless of domain. Repeatable; passing
    /// any replaces the list.
    #[arg(long = "allow-email", value_name = "EMAIL")]
    pub allow_emails: Vec<String>,

    /// A path prefix served without the gate — health endpoints, webhook
    /// receivers. Repeatable; passing any replaces the list.
    #[arg(long = "public-path", value_name = "PATH")]
    pub public_paths: Vec<String>,

    /// Where app-lb's sign-in endpoints live under this deployment's hostname.
    /// Defaults to `/__applb/auth`; set it under a path prefix if the deployment
    /// is routed by one.
    #[arg(long, value_name = "PATH")]
    pub base_path: Option<String>,

    /// How long a session lasts.
    #[arg(long = "session-ttl", value_name = "SECS")]
    pub session_ttl_secs: Option<u64>,

    /// Session cookie name.
    #[arg(long, value_name = "NAME")]
    pub cookie_name: Option<String>,

    /// Stop sending `x-auth-request-*` headers upstream.
    #[arg(long)]
    pub no_forward_identity: bool,

    /// Remove the gate; the deployment serves everyone again.
    #[arg(long, conflicts_with_all = [
        "client_id", "secret", "allow_domains", "allow_emails", "public_paths",
        "base_path", "session_ttl_secs", "cookie_name", "no_forward_identity",
    ])]
    pub clear: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_auth(ctx: &Ctx, args: &SetAuthArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    if args.clear {
        return edit_spec(ctx, &id, args.dry_run, "sign-in gate removed", |spec| {
            if let Some(map) = spec.as_object_mut() {
                map.remove("auth");
            }
            Ok(())
        });
    }

    let touched = args.client_id.is_some()
        || args.secret.is_some()
        || !args.allow_domains.is_empty()
        || !args.allow_emails.is_empty()
        || !args.public_paths.is_empty()
        || args.base_path.is_some()
        || args.session_ttl_secs.is_some()
        || args.cookie_name.is_some()
        || args.no_forward_identity;
    if !touched {
        bail!(
            "nothing to set — pass --client-id, --secret, --allow-domain, --allow-email, \
             --public-path, --base-path, --session-ttl, --cookie-name or \
             --no-forward-identity (or --clear to remove the gate)"
        );
    }

    let secret = match &args.secret {
        Some(s) => Some(spec::parse_secret_ref(s)?),
        None => None,
    };

    edit_spec(ctx, &id, args.dry_run, "sign-in gate set", |spec| {
        let auth = spec::auth_mut(spec)?;
        let fresh = auth.is_empty();
        if fresh && (args.client_id.is_none() || secret.is_none()) {
            bail!(
                "deployment {id:?} has no sign-in gate yet, so --client-id and --secret are \
                 both required (store the client secret first: `serverctl create secret \
                 google --from-stdin client_secret`)"
            );
        }
        if fresh && args.allow_domains.is_empty() && args.allow_emails.is_empty() {
            bail!(
                "a new gate needs an allow-list: --allow-domain <workspace-domain> and/or \
                 --allow-email <address>. Use --allow-domain '*' to admit any Google account"
            );
        }

        if let Some(cid) = &args.client_id {
            auth.insert("client_id".into(), Value::String(cid.clone()));
        }
        if let Some(s) = &secret {
            auth.insert("client_secret".into(), s.clone());
        }
        for (key, values) in [
            ("allowed_domains", &args.allow_domains),
            ("allowed_emails", &args.allow_emails),
            ("public_paths", &args.public_paths),
        ] {
            if !values.is_empty() {
                auth.insert(
                    key.to_string(),
                    Value::Array(values.iter().cloned().map(Value::String).collect()),
                );
            }
        }
        insert_opt_str(auth, "base_path", args.base_path.as_deref());
        insert_opt_str(auth, "cookie_name", args.cookie_name.as_deref());
        if let Some(ttl) = args.session_ttl_secs {
            auth.insert("session_ttl_secs".into(), Value::from(ttl));
        }
        if args.no_forward_identity {
            auth.insert("forward_identity".into(), Value::Bool(false));
        }
        Ok(())
    })
}

// -- update ----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// The static deployment to update, e.g. `app-obs`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Wait for the update to finish and report the outcome.
    #[arg(long, short = 'w')]
    pub wait: bool,

    /// Print the commands' output as it arrives. Implies --wait.
    #[arg(long)]
    pub logs: bool,

    #[arg(long, value_name = "SECS", default_value_t = 1800)]
    pub timeout: u64,
}

pub fn update(ctx: &Ctx, args: &UpdateArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let started = ctx.client.start_update(&id)?;

    if ctx.out.is_machine() && !(args.wait || args.logs) {
        let record: JobRecord = serde_json::from_value(started.clone()).unwrap_or_default();
        return output::emit(&started, ctx.out, &[format!("job/{}", record.id)]);
    }

    let record: JobRecord =
        serde_json::from_value(started).context("parsing the job the server started")?;
    if !ctx.out.is_machine() {
        println!(
            "job/{} started for deployment/{id} — {} command(s) in {}",
            record.id,
            record.commands_total.unwrap_or(0),
            record.working_dir.as_deref().unwrap_or("its working directory"),
        );
    }

    if !(args.wait || args.logs) {
        println!(
            "\nIt runs on the app-lb host. Follow it with `serverctl get job {}`.",
            record.id
        );
        return Ok(());
    }
    wait_job(ctx, &record.id, Duration::from_secs(args.timeout), args.logs)
}

// -- build -----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// The deployment to build, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Build this ref instead of the one in the spec. A one-off: the stored
    /// `build.ref` is left alone.
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,

    /// Wait for the build to finish and report the outcome.
    #[arg(long, short = 'w')]
    pub wait: bool,

    /// Print the build's output when it finishes. Implies --wait.
    #[arg(long)]
    pub logs: bool,

    #[arg(long, value_name = "SECS", default_value_t = 1800)]
    pub timeout: u64,
}

pub fn build(ctx: &Ctx, args: &BuildArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    let mut body = Map::new();
    if let Some(r) = &args.git_ref {
        body.insert("ref".into(), Value::String(r.clone()));
    }
    let started = ctx.client.start_build(&id, &Value::Object(body))?;

    if ctx.out.is_machine() && !(args.wait || args.logs) {
        let record: JobRecord = serde_json::from_value(started.clone()).unwrap_or_default();
        return output::emit(&started, ctx.out, &[format!("job/{}", record.id)]);
    }

    let record: JobRecord =
        serde_json::from_value(started).context("parsing the job the server started")?;
    if !ctx.out.is_machine() {
        println!(
            "job/{} started for deployment/{id} — {}",
            record.id,
            record.git_ref.as_deref().unwrap_or("(default branch)")
        );
    }

    if !(args.wait || args.logs) {
        println!(
            "\nIt runs on the app-lb host and takes as long as `docker build` does. \
             Follow it with `serverctl get job {}`.",
            record.id
        );
        return Ok(());
    }
    wait_job(ctx, &record.id, Duration::from_secs(args.timeout), args.logs)
}

/// Poll one job — build or update — to completion.
fn wait_job(ctx: &Ctx, job_id: &str, timeout: Duration, show_logs: bool) -> Result<()> {
    let started = Instant::now();
    let mut last_lines = 0usize;
    loop {
        let raw = ctx.client.get_job(job_id)?;
        let record: JobRecord =
            serde_json::from_value(raw.clone()).context("parsing the job record")?;

        // Stream whatever is new since the last poll, so a long `docker build`
        // or `cargo build` shows progress rather than a cursor.
        if show_logs && record.log.len() > last_lines {
            for line in &record.log[last_lines..] {
                println!("  {line}");
            }
            last_lines = record.log.len();
        }

        if !record.is_running() {
            if ctx.out.is_machine() {
                return output::emit(&raw, ctx.out, &[format!("job/{job_id}")]);
            }
            // With --logs the output is already on screen; reprinting the tail
            // would show every line twice for a job that failed fast.
            return report_job(&record, !show_logs);
        }
        if started.elapsed() >= timeout {
            bail!(
                "timed out after {}s waiting for job/{job_id}; it is still running on the \
                 server — check it with `serverctl get job {job_id}`",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn report_job(record: &JobRecord, show_tail: bool) -> Result<()> {
    if record.succeeded() {
        if record.is_update() {
            println!(
                "job/{} succeeded — {} ran in {}{}",
                record.id,
                record.result_summary(),
                record.working_dir.as_deref().unwrap_or("its working directory"),
                match record.verified {
                    Some(true) => "; the upstreams are answering again",
                    // The server fails a job whose upstreams never came back, so
                    // this is the verification-disabled case.
                    _ => "; the upstreams were not re-checked",
                }
            );
            return Ok(());
        }
        let rolled = if record.rolled_out {
            "; the pool is recycling onto it"
        } else {
            "; the deployment was NOT updated"
        };
        if record.is_pull() {
            println!(
                "job/{} succeeded — image {} from digest {} ({}){}",
                record.id,
                record.image.as_deref().unwrap_or("?"),
                record.short_digest(),
                match (record.bytes, record.reused) {
                    (_, true) => "already on the host".to_string(),
                    (Some(n), false) => format!("{} transferred", output::bytes(n)),
                    (None, false) => "transferred".to_string(),
                },
                rolled,
            );
        } else {
            println!(
                "job/{} succeeded — image {} from commit {}{}",
                record.id,
                record.image.as_deref().unwrap_or("?"),
                record.short_commit(),
                rolled,
            );
        }
        println!(
            "\nWatch the replacements with `serverctl rollout status {}`.",
            record.deployment
        );
        return Ok(());
    }

    // A failure ends the command non-zero, and the last few log lines are what
    // says why, so print them even without --logs.
    if show_tail {
        for line in record.log.iter().rev().take(15).collect::<Vec<_>>().into_iter().rev() {
            eprintln!("  {line}");
        }
    }
    bail!(
        "job/{} failed: {}",
        record.id,
        record.error.as_deref().unwrap_or("no reason reported")
    )
}

// -- apply -----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// A spec file (JSON or YAML), or `-` for stdin. Repeatable; a file may
    /// hold one spec, a JSON array, or a multi-document YAML stream.
    #[arg(long, short = 'f', value_name = "FILE", required = true)]
    pub filename: Vec<PathBuf>,

    /// Print what would be sent, and send nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn apply(ctx: &Ctx, args: &ApplyArgs) -> Result<()> {
    let mut specs = Vec::new();
    for path in &args.filename {
        specs.extend(spec::read_specs(path)?);
    }

    for s in &specs {
        let id = spec::spec_id(s)
            .context("a spec in the input has no `id` field")?
            .to_string();
        if args.dry_run {
            print_spec(ctx, s)?;
            continue;
        }
        // Create-or-replace, decided by what is already registered. POST would
        // also work for both (it upserts), but it tears the pool down on every
        // apply; PUT keeps VMs alive when only scaling or routing changed.
        let existed = ctx.client.deployment_exists(&id)?;
        let result = if existed {
            ctx.client.replace_deployment(&id, s)?
        } else {
            ctx.client.create_deployment(s)?
        };
        report_write(ctx, &result, &id, if existed { "configured" } else { "created" })?;
    }
    Ok(())
}

// -- edit ------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct EditArgs {
    /// The deployment to edit, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE", required = true)]
    pub args: Vec<String>,
}

pub fn edit(ctx: &Ctx, args: &EditArgs) -> Result<()> {
    let (kind, names) = parse_ref(&args.args, Some(Resource::Deployment))?;
    if kind != Resource::Deployment {
        bail!("edit only works on deployments");
    }
    let [id] = names.as_slice() else {
        bail!("edit takes exactly one deployment");
    };

    let current = spec::spec_of(&ctx.client.get_deployment(id)?)?;
    let header = format!(
        "# Editing deployment {id:?} on {}.\n\
         # Save an unchanged file (or an empty one) to cancel.\n\
         # The pool is preserved unless the `vm` block or `upstreams` change.\n",
        ctx.endpoint.server
    );
    let original = format!("{header}{}", serde_yaml::to_string(&current)?);

    let path = std::env::temp_dir().join(format!(
        "serverctl-{}-{}.yaml",
        sanitize(id),
        std::process::id()
    ));
    std::fs::write(&path, &original).with_context(|| format!("writing {}", path.display()))?;

    let edited = run_editor(&path).inspect_err(|_| {
        let _ = std::fs::remove_file(&path);
    })?;

    if edited == original || edited.lines().all(|l| l.trim().is_empty() || l.starts_with('#')) {
        std::fs::remove_file(&path).ok();
        println!("Edit cancelled, no changes made.");
        return Ok(());
    }

    let new_spec: Value = serde_yaml::from_str(&edited)
        .with_context(|| format!("the edited spec is not valid YAML/JSON; it is kept at {}", path.display()))?;

    match ctx.client.replace_deployment(id, &new_spec) {
        Ok(result) => {
            std::fs::remove_file(&path).ok();
            report_write(ctx, &result, id, "edited")
        }
        // Keep the buffer on rejection: retyping a spec because the server said
        // "min_replicas exceeds max_replicas" would be a poor trade.
        Err(e) => Err(e).with_context(|| format!("your edit is kept at {}", path.display())),
    }
}

fn run_editor(path: &std::path::Path) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // Through a shell, so EDITOR="code -w" and friends keep working. The path
    // goes in as $1 rather than being interpolated, so a space in $TMPDIR
    // doesn't split into two filenames.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("launching the editor ({editor})"))?;
    if !status.success() {
        bail!("the editor ({editor}) exited with {status}");
    }
    std::fs::read_to_string(path).with_context(|| format!("reading back {}", path.display()))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

// -- set -------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct SetImageArgs {
    /// The deployment, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// The new guest image.
    #[arg(value_name = "IMAGE")]
    pub image: String,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct SetEnvArgs {
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// `KEY=VALUE` to set, `KEY-` to remove. Repeatable.
    #[arg(value_name = "KEY=VALUE", required = true)]
    pub changes: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct SetUpstreamsArgs {
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// The full new upstream list, as `host:port` addresses.
    #[arg(value_name = "ADDR", required = true)]
    pub upstreams: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct SetRouteArgs {
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// Exact hostname to route.
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,
    /// Subdomain match: the apex and any subdomain of it.
    #[arg(long, value_name = "DOMAIN")]
    pub host_suffix: Option<String>,
    /// Path prefix to route.
    #[arg(long, value_name = "PATH")]
    pub path_prefix: Option<String>,
    /// A rule in `host=…,path=…` form. Repeatable.
    #[arg(long = "route", value_name = "RULE")]
    pub routes: Vec<String>,
    /// Keep the existing rules and add these, instead of replacing them.
    #[arg(long)]
    pub add: bool,
    /// Remove every route, withdrawing the deployment from the proxy. The
    /// counterpart to `create --no-route`: an exposed sandbox goes back to
    /// being reachable only by exec/shell, without being torn down. Managed
    /// (VM) deployments only.
    #[arg(long, conflicts_with_all = ["host", "host_suffix", "path_prefix", "routes", "add"])]
    pub none: bool,
    #[arg(long)]
    pub dry_run: bool,
}

pub fn set_image(ctx: &Ctx, args: &SetImageArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    // A new image means new VMs: app-lb rebuilds the pool when the `vm` block
    // changes, so this is the closest thing to a rolling deploy.
    edit_spec(ctx, &id, args.dry_run, "image updated", |spec| {
        let vm = spec::vm_mut(spec, &id)?;
        vm.insert("image".into(), Value::String(args.image.clone()));
        Ok(())
    })
}

pub fn set_env(ctx: &Ctx, args: &SetEnvArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let changes: Vec<EnvChange> = args
        .changes
        .iter()
        .map(|c| spec::parse_env(c))
        .collect::<Result<_>>()?;
    edit_spec(ctx, &id, args.dry_run, "env updated", |spec| {
        spec::apply_env(spec, &id, &changes)?;
        Ok(())
    })
}

pub fn set_upstreams(ctx: &Ctx, args: &SetUpstreamsArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    edit_spec(ctx, &id, args.dry_run, "upstreams updated", |spec| {
        if !spec::is_static(spec) {
            bail!(
                "deployment {id:?} is a managed VM pool, not a static proxy_pass one — \
                 its backends come from the `vm` template, not an upstream list"
            );
        }
        spec["upstreams"] = Value::Array(args.upstreams.iter().cloned().map(Value::String).collect());
        Ok(())
    })
}

pub fn set_route(ctx: &Ctx, args: &SetRouteArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let mut rules = Vec::new();
    if let Some(rule) = spec::route_from_parts(
        args.host.as_deref(),
        args.host_suffix.as_deref(),
        args.path_prefix.as_deref(),
    ) {
        rules.push(rule);
    }
    for r in &args.routes {
        rules.push(spec::parse_route(r)?);
    }
    if rules.is_empty() && !args.none {
        bail!(
            "nothing to set — pass --host, --host-suffix, --path-prefix or --route, \
             or --none to withdraw the deployment from the proxy"
        );
    }

    let message = if args.none { "routes cleared" } else { "routes updated" };
    edit_spec(ctx, &id, args.dry_run, message, |spec| {
        if args.none && spec::is_static(spec) {
            bail!(
                "deployment {id:?} is a static (proxy_pass) one, and the proxy is the \
                 only way to reach it — clearing its routes would leave it unreachable"
            );
        }
        let existing = spec
            .get("routes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut next = if args.add { existing } else { Vec::new() };
        for rule in &rules {
            if !next.contains(rule) {
                next.push(rule.clone());
            }
        }
        spec["routes"] = Value::Array(next);
        Ok(())
    })
}

/// Read-modify-write one deployment's spec.
fn edit_spec(
    ctx: &Ctx,
    id: &str,
    dry_run: bool,
    verb: &str,
    mutate: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let mut spec = spec::spec_of(&ctx.client.get_deployment(id)?)?;
    mutate(&mut spec)?;
    if dry_run {
        return print_spec(ctx, &spec);
    }
    let result = ctx.client.replace_deployment(id, &spec)?;
    report_write(ctx, &result, id, verb)
}

// -- scale -----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct ScaleArgs {
    /// The deployment to scale, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Pin the pool to exactly N replicas (sets both min and max). Without it,
    /// the autoscaler keeps deciding within the min/max band.
    #[arg(long, short = 'r', value_name = "N", conflicts_with_all = ["min", "max"])]
    pub replicas: Option<u64>,

    #[command(flatten)]
    pub scaling: ScalingFlags,
}

pub fn scale(ctx: &Ctx, args: &ScaleArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let mut patch = args.scaling.patch();
    if let Some(n) = args.replicas {
        patch.insert("min_replicas".into(), Value::from(n));
        patch.insert("max_replicas".into(), Value::from(n));
    }
    if patch.is_empty() {
        bail!(
            "nothing to change — pass --replicas, or one of --min/--max/--warm/\
             --target-concurrency/--scale-to-zero-after/--cold-start-timeout/--drain-timeout/\
             --boot-timeout/--idle-action"
        );
    }

    let result = ctx.client.patch_scaling(&id, &Value::Object(patch))?;
    if ctx.out.is_machine() {
        return output::emit(&result, ctx.out, &[format!("deployment/{id}")]);
    }
    let status: DeploymentStatus = serde_json::from_value(result)?;
    println!(
        "deployment/{id} scaled — desired {} (min {}, max {}, warm {}, target {} in-flight/VM), \
         {} ready, {} pending",
        status.desired_replicas,
        status.spec.scaling.min_replicas,
        status.spec.scaling.max_replicas,
        status.spec.scaling.warm_pool,
        status.spec.scaling.target_concurrency,
        status.ready,
        status.pending,
    );
    Ok(())
}

// -- restart / rollout status ---------------------------------------------

#[derive(Args, Debug)]
pub struct RestartArgs {
    /// The deployment to recycle, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// Kill VMs immediately, dropping in-flight requests, instead of draining.
    #[arg(long)]
    pub force: bool,
    /// Wait until the replacement pool is ready.
    #[arg(long)]
    pub wait: bool,
    #[arg(long, value_name = "SECS", default_value_t = 300)]
    pub timeout: u64,
}

pub fn restart(ctx: &Ctx, args: &RestartArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let status: DeploymentStatus = serde_json::from_value(ctx.client.get_deployment(&id)?)?;

    if status.spec.is_static() {
        bail!(
            "deployment {id:?} is static (proxy_pass) — it has no VMs to recycle. \
             Restart the upstream itself; app-lb re-probes it every tick and it rejoins."
        );
    }
    if status.vms.is_empty() {
        println!("deployment/{id} has no running VMs — nothing to restart.");
        return Ok(());
    }

    // Evicting is per-VM: there is no rollout object server-side. The autoscaler
    // boots replacements on its next tick, so this is a recycle, not a shrink.
    println!(
        "Recycling {} VM(s) of deployment/{id} ({}).",
        status.vms.len(),
        if args.force { "killing now" } else { "draining" }
    );
    let mut table = Table::new(["SANDBOX", "OUTCOME"]);
    for vm in &status.vms {
        let result = ctx.client.evict_vm(&id, &vm.sandbox_id, args.force)?;
        let outcome = result
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("requested")
            .to_string();
        table.row([vm.sandbox_id.clone(), outcome]);
    }
    table.print();

    if args.wait {
        println!();
        return wait_ready(ctx, &id, Duration::from_secs(args.timeout));
    }
    println!("\nWatch the replacements with `serverctl rollout status {id}`.");
    Ok(())
}

#[derive(Args, Debug)]
pub struct RolloutStatusArgs {
    /// The deployment to watch, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,
    /// Give up after this long.
    #[arg(long, value_name = "SECS", default_value_t = 300)]
    pub timeout: u64,
    /// Print the current state once and exit.
    #[arg(long)]
    pub no_wait: bool,
}

pub fn rollout_status(ctx: &Ctx, args: &RolloutStatusArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    if args.no_wait {
        let status: DeploymentStatus = serde_json::from_value(ctx.client.get_deployment(&id)?)?;
        println!("{}", describe_progress(&status));
        return Ok(());
    }
    wait_ready(ctx, &id, Duration::from_secs(args.timeout))
}

/// Poll until the pool matches its desired size with every VM healthy.
fn wait_ready(ctx: &Ctx, id: &str, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    let mut last = String::new();
    loop {
        let status: DeploymentStatus = serde_json::from_value(ctx.client.get_deployment(id)?)?;
        let line = describe_progress(&status);
        if line != last {
            println!("{line}");
            last = line;
        }

        let healthy = status.vms.iter().filter(|v| v.healthy && !v.draining).count();
        if status.pending == 0
            && healthy >= status.desired_replicas as usize
            && !status.vms.iter().any(|v| v.draining)
        {
            println!("deployment/{id} is ready.");
            return Ok(());
        }
        if started.elapsed() >= timeout {
            bail!(
                "timed out after {}s waiting for deployment/{id} — \
                 `serverctl describe deployment {id}` shows where it is stuck",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn describe_progress(s: &DeploymentStatus) -> String {
    let healthy = s.vms.iter().filter(|v| v.healthy && !v.draining).count();
    let draining = s.vms.iter().filter(|v| v.draining).count();
    format!(
        "Waiting for deployment/{}: {healthy}/{} ready, {} pending, {draining} draining",
        s.spec.id, s.desired_replicas, s.pending
    )
}

// -- delete ----------------------------------------------------------------

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// What to delete: `deployment web`, `deployment/web`, or
    /// `vm <sandbox-id> --deployment web`.
    #[arg(value_name = "RESOURCE", required = true)]
    pub args: Vec<String>,

    /// The deployment a VM belongs to (required when deleting VMs).
    #[arg(long, short = 'd', value_name = "NAME")]
    pub deployment: Option<String>,

    /// Delete every deployment, or every VM of --deployment.
    #[arg(long)]
    pub all: bool,

    /// For VMs: kill immediately, dropping in-flight requests, instead of
    /// draining.
    #[arg(long)]
    pub force: bool,

    /// Skip the confirmation prompt for destructive bulk deletes.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn delete(ctx: &Ctx, args: &DeleteArgs) -> Result<()> {
    let (kind, names) = parse_ref(&args.args, None)?;
    match kind {
        Resource::Deployment => delete_deployments(ctx, &names, args),
        Resource::Vm => delete_vms(ctx, &names, args),
        Resource::Secret => delete_secrets(ctx, &names, args),
        Resource::Cert => bail!(
            "certificates are managed by app-lb's ACME loop and cannot be deleted through \
             the API; remove them from APP_LB_ACME_DIR on the host instead"
        ),
        Resource::Job => bail!(
            "jobs are a record of something that already happened and cannot be deleted; \
             the server keeps the most recent ones and forgets the rest"
        ),
        Resource::All => bail!("`delete all` is not supported — name the deployments, or use --all"),
    }
}

fn delete_secrets(ctx: &Ctx, names: &[String], args: &DeleteArgs) -> Result<()> {
    let targets: Vec<String> = if args.all {
        let list: Vec<SecretSummary> = serde_json::from_value(ctx.client.list_secrets()?)?;
        list.into_iter().map(|s| s.id).collect()
    } else {
        if names.is_empty() {
            bail!("delete secret needs a name, or --all");
        }
        names.to_vec()
    };

    if targets.is_empty() {
        println!("No secrets to delete.");
        return Ok(());
    }
    if args.all && !args.yes {
        confirm(&format!(
            "About to delete {} secret(s), permanently: {}",
            targets.len(),
            targets.join(", ")
        ))?;
    }

    for id in &targets {
        // The server refuses (409) while a deployment's build still references
        // the secret; --force is how you say you meant it.
        ctx.client.delete_secret(id, args.force)?;
        println!("secret/{id} deleted");
    }
    Ok(())
}

fn delete_deployments(ctx: &Ctx, names: &[String], args: &DeleteArgs) -> Result<()> {
    let targets: Vec<String> = if args.all {
        let list: Vec<DeploymentStatus> = serde_json::from_value(ctx.client.list_deployments()?)?;
        list.into_iter().map(|d| d.spec.id).collect()
    } else {
        if names.is_empty() {
            bail!("delete needs a name, or --all");
        }
        names.to_vec()
    };

    if targets.is_empty() {
        println!("No deployments to delete.");
        return Ok(());
    }
    if args.all && !args.yes {
        confirm(&format!(
            "About to delete {} deployment(s) and tear down their VMs: {}",
            targets.len(),
            targets.join(", ")
        ))?;
    }

    for id in &targets {
        ctx.client.delete_deployment(id)?;
        println!("deployment/{id} deleted");
    }
    Ok(())
}

fn delete_vms(ctx: &Ctx, names: &[String], args: &DeleteArgs) -> Result<()> {
    let deployment = args
        .deployment
        .clone()
        .context("deleting a VM needs --deployment (VM ids are scoped to a deployment)")?;
    let id = deployment_name(&deployment)?;

    let targets: Vec<String> = if args.all {
        let status: DeploymentStatus = serde_json::from_value(ctx.client.get_deployment(&id)?)?;
        status.vms.into_iter().map(|v| v.sandbox_id).collect()
    } else {
        if names.is_empty() {
            bail!("delete vm needs a sandbox id, or --all");
        }
        names.to_vec()
    };

    if targets.is_empty() {
        println!("No VMs to delete in deployment/{id}.");
        return Ok(());
    }

    for sandbox in &targets {
        let result = ctx.client.evict_vm(&id, sandbox, args.force)?;
        let outcome = result
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("requested");
        println!("vm/{sandbox} {outcome}");
    }
    // Worth saying, because it is the difference between this and `scale`.
    println!(
        "\nThe autoscaler boots replacements on its next tick if the scaling policy still \
         wants the capacity. To shrink the pool, use `serverctl scale`."
    );
    Ok(())
}

fn confirm(prompt: &str) -> Result<()> {
    println!("{prompt}");
    print!("Type 'yes' to continue: ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading confirmation")?;
    if answer.trim() != "yes" {
        bail!("aborted");
    }
    Ok(())
}

// -- shared reporting ------------------------------------------------------

fn print_spec(ctx: &Ctx, spec: &Value) -> Result<()> {
    match ctx.out {
        crate::output::OutputFormat::Yaml => print!("{}", serde_yaml::to_string(spec)?),
        _ => println!("{}", serde_json::to_string_pretty(spec)?),
    }
    Ok(())
}

/// The one-line confirmation after a write, or the server's object under
/// `-o json`/`-o yaml`.
fn report_write(ctx: &Ctx, result: &Value, id: &str, verb: &str) -> Result<()> {
    if ctx.out.is_machine() {
        return output::emit(result, ctx.out, &[format!("deployment/{id}")]);
    }
    let status: Option<DeploymentStatus> = serde_json::from_value(result.clone()).ok();
    match status {
        Some(s) if !s.spec.is_static() => println!(
            "deployment/{id} {verb} — desired {}, {} ready, {} pending",
            s.desired_replicas, s.ready, s.pending
        ),
        Some(s) => println!(
            "deployment/{id} {verb} — {} upstream(s), {} healthy",
            s.spec.upstreams.len(),
            s.ready
        ),
        None => println!("deployment/{id} {verb}"),
    }
    Ok(())
}
