//! The commands that change something: create, apply, edit, set, scale,
//! restart and delete.
//!
//! Every in-place change is a read-modify-write against `PUT /deployments/:id`,
//! which replaces the whole spec. The one exception is `scale`, which has a
//! real partial endpoint (`PATCH .../scaling`) and so never has to read first.

use super::{Ctx, Resource, deployment_name, parse_ref};
use crate::output::{self, Table};
use crate::spec::{self, EnvChange};
use crate::types::DeploymentStatus;
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
}

impl ScalingFlags {
    fn patch(&self) -> Map<String, Value> {
        spec::scaling_patch(&[
            ("min_replicas", self.min),
            ("max_replicas", self.max),
            ("warm_pool", self.warm),
            ("target_concurrency", self.target_concurrency),
            ("scale_to_zero_after_secs", self.scale_to_zero_after),
            ("cold_start_timeout_secs", self.cold_start_timeout),
            ("drain_timeout_secs", self.drain_timeout),
        ])
    }
}

pub fn create(ctx: &Ctx, args: &CreateDeploymentArgs) -> Result<()> {
    let spec = build_spec(args)?;
    if args.dry_run {
        return print_spec(ctx, &spec);
    }
    let created = ctx.client.create_deployment(&spec)?;
    report_write(ctx, &created, &args.name, "created")
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
    if routes.is_empty() {
        bail!(
            "a deployment needs at least one route — pass --host, --host-suffix, \
             --path-prefix or --route"
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
    if rules.is_empty() {
        bail!("nothing to set — pass --host, --host-suffix, --path-prefix or --route");
    }

    edit_spec(ctx, &id, args.dry_run, "routes updated", |spec| {
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
             --target-concurrency/--scale-to-zero-after/--cold-start-timeout/--drain-timeout"
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
        Resource::Cert => bail!(
            "certificates are managed by app-lb's ACME loop and cannot be deleted through \
             the API; remove them from APP_LB_ACME_DIR on the host instead"
        ),
        Resource::All => bail!("`delete all` is not supported — name the deployments, or use --all"),
    }
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
