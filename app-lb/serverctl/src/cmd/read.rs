//! `get` and `describe` — the read-only views of the control plane.

use super::{Ctx, Resource, parse_ref};
use crate::output::{self, OutputFormat, Table};
use crate::types::{CertStatus, DeploymentStatus, MetricsResponse};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::Value;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct GetArgs {
    /// What to list: deployments, vms, certs, or all. Accepts `deployment/web`
    /// and trailing names, e.g. `get deploy web api`.
    #[arg(value_name = "RESOURCE", required = true)]
    pub args: Vec<String>,

    /// Only show VMs belonging to this deployment.
    #[arg(long, short = 'd', value_name = "NAME")]
    pub deployment: Option<String>,

    /// Re-render every --interval seconds until interrupted.
    #[arg(long, short = 'w')]
    pub watch: bool,

    #[arg(long, value_name = "SECS", default_value_t = 2, requires = "watch")]
    pub interval: u64,
}

pub fn get(ctx: &Ctx, args: &GetArgs) -> Result<()> {
    let (kind, names) = parse_ref(&args.args, None)?;
    if args.watch {
        if ctx.out.is_machine() {
            bail!("--watch renders a table; drop `-o {}`", format_name(ctx.out));
        }
        return watch(Duration::from_secs(args.interval.max(1)), || {
            get_once(ctx, kind, &names, args)
        });
    }
    get_once(ctx, kind, &names, args)
}

fn get_once(ctx: &Ctx, kind: Resource, names: &[String], args: &GetArgs) -> Result<()> {
    match kind {
        Resource::Deployment => get_deployments(ctx, names),
        Resource::Vm => get_vms(ctx, names, args.deployment.as_deref()),
        Resource::Cert => get_certs(ctx),
        Resource::All => {
            get_deployments(ctx, &[])?;
            println!();
            get_vms(ctx, &[], None)
        }
    }
}

/// Fetch either the whole list or the named subset, as raw JSON plus the parsed
/// view. Both are kept: `-o json` must print the server's own bytes.
fn fetch_deployments(ctx: &Ctx, names: &[String]) -> Result<(Value, Vec<DeploymentStatus>)> {
    let raw = if names.is_empty() {
        ctx.client.list_deployments()?
    } else {
        let mut out = Vec::new();
        for name in names {
            out.push(
                ctx.client
                    .get_deployment(name)
                    .with_context(|| format!("getting deployment {name:?}"))?,
            );
        }
        // A single name prints as one object, several as a list — the shape
        // `-o json` consumers expect from the equivalent API calls.
        if out.len() == 1 {
            out.into_iter().next().expect("len == 1")
        } else {
            Value::Array(out)
        }
    };

    let parsed: Vec<DeploymentStatus> = match &raw {
        Value::Array(items) => items
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<_, _>>()
            .context("parsing the deployment list")?,
        other => vec![serde_json::from_value(other.clone()).context("parsing the deployment")?],
    };
    Ok((raw, parsed))
}

fn get_deployments(ctx: &Ctx, names: &[String]) -> Result<()> {
    let (raw, deployments) = fetch_deployments(ctx, names)?;

    if ctx.out.is_machine() {
        let names: Vec<String> = deployments
            .iter()
            .map(|d| format!("deployment/{}", d.spec.id))
            .collect();
        return output::emit(&raw, ctx.out, &names);
    }

    if deployments.is_empty() {
        println!("No deployments registered.");
        return Ok(());
    }

    let mut table = if ctx.out.is_wide() {
        Table::new([
            "NAME", "KIND", "ROUTES", "DESIRED", "READY", "PENDING", "IN-FLIGHT", "MIN", "MAX",
            "WARM", "TARGET", "BACKEND",
        ])
    } else {
        Table::new(["NAME", "KIND", "ROUTES", "DESIRED", "READY", "PENDING", "IN-FLIGHT"])
    };

    for d in &deployments {
        let mut row = vec![
            d.spec.id.clone(),
            d.kind.clone(),
            d.spec.routes_summary(),
            // A static deployment has no autoscaler, so "desired" is a number
            // nobody set — show its upstream count instead of a misleading 0.
            if d.spec.is_static() {
                "—".to_string()
            } else {
                d.desired_replicas.to_string()
            },
            d.ready.to_string(),
            d.pending.to_string(),
            d.total_in_flight.to_string(),
        ];
        if ctx.out.is_wide() {
            let s = &d.spec.scaling;
            if d.spec.is_static() {
                row.extend(["—".to_string(), "—".to_string(), "—".to_string(), "—".to_string()]);
            } else {
                row.extend([
                    s.min_replicas.to_string(),
                    s.max_replicas.to_string(),
                    s.warm_pool.to_string(),
                    s.target_concurrency.to_string(),
                ]);
            }
            row.push(d.spec.backend_summary());
        }
        table.row(row);
    }
    table.print();
    Ok(())
}

fn get_vms(ctx: &Ctx, names: &[String], filter: Option<&str>) -> Result<()> {
    let scope: Vec<String> = filter.into_iter().map(str::to_string).collect();
    let (_, deployments) = fetch_deployments(ctx, &scope)?;

    let wanted = |id: &str| names.is_empty() || names.iter().any(|n| n == id);

    if ctx.out.is_machine() {
        let mut rows = Vec::new();
        let mut refs = Vec::new();
        for d in &deployments {
            for vm in d.vms.iter().filter(|v| wanted(&v.sandbox_id)) {
                refs.push(format!("vm/{}", vm.sandbox_id));
                rows.push(serde_json::json!({
                    "deployment": d.spec.id,
                    "sandbox_id": vm.sandbox_id,
                    "addr": vm.addr,
                    "in_flight": vm.in_flight,
                    "healthy": vm.healthy,
                    "draining": vm.draining,
                }));
            }
        }
        return output::emit(&Value::Array(rows), ctx.out, &refs);
    }

    let mut table = Table::new(["DEPLOYMENT", "SANDBOX", "ADDRESS", "STATUS", "IN-FLIGHT"]);
    for d in &deployments {
        for vm in d.vms.iter().filter(|v| wanted(&v.sandbox_id)) {
            table.row([
                d.spec.id.clone(),
                vm.sandbox_id.clone(),
                vm.addr.clone(),
                vm.status().to_string(),
                vm.in_flight.to_string(),
            ]);
        }
    }
    if table.is_empty() {
        println!("No VMs in the pool. (`serverctl top vms` shows resource usage for running VMs.)");
        return Ok(());
    }
    table.print();
    Ok(())
}

fn get_certs(ctx: &Ctx) -> Result<()> {
    let raw = ctx.client.certs()?;
    let certs: Vec<CertStatus> =
        serde_json::from_value(raw.clone()).context("parsing the certificate list")?;

    if ctx.out.is_machine() {
        let names: Vec<String> = certs.iter().map(|c| format!("cert/{}", c.host)).collect();
        return output::emit(&raw, ctx.out, &names);
    }

    if certs.is_empty() {
        println!(
            "No certificates issued. (ACME is off unless APP_LB_ACME_EMAIL is set; \
             `host_suffix` routes cannot be covered by ACME.)"
        );
        return Ok(());
    }

    let mut table = Table::new(["HOST", "ISSUER", "NOT-AFTER", "RENEWAL"]);
    for c in &certs {
        table.row([
            c.host.clone(),
            c.issuer.clone(),
            c.not_after.clone(),
            if c.needs_renewal { "due".into() } else { "ok".to_string() },
        ]);
    }
    table.print();
    Ok(())
}

#[derive(Args, Debug)]
pub struct DescribeArgs {
    /// The deployment to describe, e.g. `web` or `deployment/web`.
    #[arg(value_name = "RESOURCE", required = true)]
    pub args: Vec<String>,
}

pub fn describe(ctx: &Ctx, args: &DescribeArgs) -> Result<()> {
    let (kind, names) = parse_ref(&args.args, Some(Resource::Deployment))?;
    if kind != Resource::Deployment {
        bail!("describe only works on deployments");
    }
    if names.is_empty() {
        bail!("describe needs a name, e.g. `serverctl describe deployment web`");
    }

    // Metrics are a separate, separately-gated endpoint. Fetch once, and carry
    // on without them if this user can only reach the CRUD API. Not needed at
    // all under -o json/yaml, which prints the deployment payload verbatim.
    let metrics: Option<MetricsResponse> = if ctx.out.is_machine() {
        None
    } else {
        ctx.client
            .metrics()
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
    };

    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let raw = ctx.client.get_deployment(name)?;
        if ctx.out.is_machine() {
            output::emit(&raw, ctx.out, &[format!("deployment/{name}")])?;
            continue;
        }
        let d: DeploymentStatus = serde_json::from_value(raw)?;
        describe_one(&d, metrics.as_ref());
    }
    Ok(())
}

fn describe_one(d: &DeploymentStatus, metrics: Option<&MetricsResponse>) {
    output::top_field("Name", &d.spec.id);
    output::top_field(
        "Kind",
        if d.spec.is_static() {
            "static (proxy_pass to fixed upstreams)".to_string()
        } else {
            let driver = d.spec.vm.as_ref().map(|v| v.driver.clone()).unwrap_or_default();
            format!("vm (managed {driver} pool)")
        },
    );

    output::section("Routes");
    if d.spec.routes.is_empty() {
        println!("  <none>");
    }
    for r in &d.spec.routes {
        println!("  {}", r.render());
    }

    if d.spec.is_static() {
        output::section("Upstreams");
        for u in &d.spec.upstreams {
            println!("  {u}");
        }
    } else if let Some(vm) = &d.spec.vm {
        output::section("VM template");
        output::field("Driver", &vm.driver);
        output::field("Image", output::opt_str(vm.image.as_deref()));
        output::field("Port", vm.port.to_string());
        output::field("Size class", output::opt_str(vm.size_class.as_deref()));
        if let Some(gb) = vm.disk_size_gb {
            output::field("Disk", format!("{gb} GB"));
        }
        if let Some(cmd) = &vm.start_command {
            output::field("Start command", cmd);
        }
        if let Some(dir) = &vm.working_directory {
            output::field("Working dir", dir);
        }
        if !vm.open_ports.is_empty() {
            let ports: Vec<String> = vm.open_ports.iter().map(u16::to_string).collect();
            output::field("Open ports", ports.join(","));
        }
        output::field("TTL", output::duration(vm.ttl_seconds));
        if let Some(env) = &vm.env_vars {
            // Values can be secrets (the API echoes the whole spec back), so
            // show the keys and let `-o json` be the deliberate way to see them.
            let keys: Vec<&str> = env.keys().map(String::as_str).collect();
            output::field(
                "Env",
                if keys.is_empty() {
                    "<none>".to_string()
                } else {
                    format!("{} ({})", keys.join(", "), "values hidden; use -o json")
                },
            );
        }
        if let Some(hooks) = &vm.setup_hooks
            && !hooks.is_empty()
        {
            output::field("Setup hooks", hooks.join(" ; "));
        }

        output::section("Scaling");
        let s = &d.spec.scaling;
        output::field("Desired", d.desired_replicas.to_string());
        output::field("Min / Max", format!("{} / {}", s.min_replicas, s.max_replicas));
        output::field("Warm pool", s.warm_pool.to_string());
        output::field("Target concurrency", s.target_concurrency.to_string());
        output::field(
            "Scale to zero after",
            output::duration(s.scale_to_zero_after_secs),
        );
        output::field("Cold start timeout", output::duration(s.cold_start_timeout_secs));
        output::field("Drain timeout", output::duration(s.drain_timeout_secs));
    }

    output::section("Health check");
    output::field(
        "Probe",
        match &d.spec.health.path {
            Some(p) => format!("GET {p}"),
            None => "TCP connect".to_string(),
        },
    );
    if let Some(port) = d.spec.health.port {
        output::field("Port", port.to_string());
    }
    output::field("Timeout", output::duration(d.spec.health.timeout_secs));

    output::section("Backends");
    output::field("Ready / Pending", format!("{} / {}", d.ready, d.pending));
    output::field("In flight", d.total_in_flight.to_string());
    if d.vms.is_empty() {
        println!("  (no backends)");
    } else if d.spec.is_static() {
        // A static deployment's "backends" are the configured upstreams — there
        // is no sandbox, and no resource usage to report for one.
        let mut table = Table::indented(["UPSTREAM", "STATUS", "IN-FLIGHT"], 2);
        for vm in &d.vms {
            table.row([
                vm.addr.clone(),
                vm.status().to_string(),
                vm.in_flight.to_string(),
            ]);
        }
        table.print();
    } else {
        let view = metrics.and_then(|m| m.deployments.iter().find(|v| v.id == d.spec.id));
        let mut table = Table::indented(
            ["SANDBOX", "ADDRESS", "STATUS", "IN-FLIGHT", "CPU%", "MEMORY", "UPTIME"],
            2,
        );
        for vm in &d.vms {
            let live = view.and_then(|v| v.vms.iter().find(|x| x.sandbox_id == vm.sandbox_id));
            table.row([
                vm.sandbox_id.clone(),
                vm.addr.clone(),
                vm.status().to_string(),
                vm.in_flight.to_string(),
                output::opt_percent(live.and_then(|l| l.cpu_percent)),
                output::opt_bytes(live.and_then(|l| l.memory_bytes)),
                live.map(|l| output::duration(l.uptime_secs))
                    .unwrap_or_else(|| "—".into()),
            ]);
        }
        table.print();
    }

    match metrics.and_then(|m| m.deployments.iter().find(|v| v.id == d.spec.id)) {
        None => {
            output::section("Traffic");
            println!("  (no metrics — /metrics is gated or unreachable for this user)");
        }
        Some(view) => {
            let m = &view.metrics;
            output::section("Traffic (since app-lb started)");
            output::field(
                "Requests",
                format!(
                    "{} total — {} 2xx, {} 3xx, {} 4xx, {} 5xx, {} errors",
                    m.requests.total,
                    m.requests.c2xx,
                    m.requests.c3xx,
                    m.requests.c4xx,
                    m.requests.c5xx,
                    m.requests.errors
                ),
            );
            output::field(
                "Latency",
                format!(
                    "p50 {}  p90 {}  p99 {}",
                    output::millis(m.latency_ms.p50),
                    output::millis(m.latency_ms.p90),
                    output::millis(m.latency_ms.p99)
                ),
            );
            output::field("Utilization", output::ratio_percent(view.pool.utilization));
            output::field(
                "Pool CPU / memory",
                format!(
                    "{} / {}",
                    output::opt_percent(view.pool.cpu_percent),
                    output::opt_bytes(view.pool.memory_bytes)
                ),
            );
            let a = &m.autoscale;
            output::field(
                "Autoscaler",
                format!(
                    "{} VMs created, {} drained, {} reaped ({} up / {} down events)",
                    a.vms_created, a.vms_drained, a.vms_reaped, a.scale_up_events, a.scale_down_events
                ),
            );
            output::field(
                "Cold starts",
                format!(
                    "{} waits — {} served, {} timed out (p50 {:.1}s)",
                    a.cold_start_waits, a.cold_start_hits, a.cold_start_timeouts, m.cold_start_s.p50
                ),
            );
        }
    }
}

/// Re-run a renderer on a timer, clearing the screen between passes.
pub fn watch(interval: Duration, mut render: impl FnMut() -> Result<()>) -> Result<()> {
    loop {
        // Home the cursor and clear, so successive frames don't scroll.
        print!("\x1b[2J\x1b[H");
        render()?;
        println!("\n(watching every {}s — Ctrl-C to stop)", interval.as_secs());
        std::thread::sleep(interval);
    }
}

/// Shared by `get`/`top`: how the output format was spelled, for error text.
pub fn format_name(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::Table => "table",
        OutputFormat::Wide => "wide",
        OutputFormat::Json => "json",
        OutputFormat::Yaml => "yaml",
        OutputFormat::Name => "name",
    }
}
