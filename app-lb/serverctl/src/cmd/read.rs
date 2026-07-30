//! `get` and `describe` — the read-only views of the control plane.

use super::{Ctx, Resource, parse_ref};
use crate::output::{self, OutputFormat, Table};
use crate::types::{CertStatus, DeploymentStatus, JobRecord, MetricsResponse, SecretSummary};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::Value;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct GetArgs {
    /// What to list: deployments, vms, certs, secrets, jobs, or all. Accepts
    /// `deployment/web` and trailing names, e.g. `get deploy web api`.
    #[arg(value_name = "RESOURCE", required = true)]
    pub args: Vec<String>,

    /// Only show VMs or jobs belonging to this deployment.
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
        Resource::Secret => get_secrets(ctx, names),
        Resource::Job => get_jobs(ctx, names, args.deployment.as_deref()),
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
            "WARM", "TARGET", "BACKEND", "SOURCE", "AUTH",
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
            // How this deployment is updated, which the BACKEND column (what is
            // running now) deliberately does not say. A managed deployment
            // builds from a repo or pulls from an artifact store — never both,
            // which is what lets one column carry either; a static one runs
            // commands on the host.
            row.push(match (&d.spec.build, &d.spec.artifact, &d.spec.update) {
                (Some(b), _, _) => b.summary(),
                (None, Some(a), _) => a.summary(),
                (None, None, Some(u)) => u.summary(),
                (None, None, None) => "—".into(),
            });
            // Whether anything stands in front of this deployment at all — the
            // one property you want to be able to scan a whole fleet for.
            row.push(match &d.spec.auth {
                Some(a) if a.provider.is_empty() => "google".to_string(),
                Some(a) => a.provider.clone(),
                None => "—".into(),
            });
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

/// `get secrets` — names and key names. There is no flag that prints a value:
/// app-lb has no endpoint that returns one.
fn get_secrets(ctx: &Ctx, names: &[String]) -> Result<()> {
    let raw = if names.is_empty() {
        ctx.client.list_secrets()?
    } else {
        let mut out = Vec::new();
        for name in names {
            out.push(
                ctx.client
                    .get_secret(name)
                    .with_context(|| format!("getting secret {name:?}"))?,
            );
        }
        Value::Array(out)
    };

    let secrets: Vec<SecretSummary> = match &raw {
        Value::Array(items) => items
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<_, _>>()
            .context("parsing the secret list")?,
        other => vec![serde_json::from_value(other.clone()).context("parsing the secret")?],
    };

    if ctx.out.is_machine() {
        let refs: Vec<String> = secrets.iter().map(|s| format!("secret/{}", s.id)).collect();
        return output::emit(&raw, ctx.out, &refs);
    }

    if secrets.is_empty() {
        println!(
            "No secrets stored. (`serverctl create secret github --from-stdin token` \
             stores one; values are never readable back.)"
        );
        return Ok(());
    }

    let mut table = if ctx.out.is_wide() {
        Table::new(["NAME", "KEYS", "AT-REST", "UPDATED", "DESCRIPTION"])
    } else {
        Table::new(["NAME", "KEYS", "UPDATED"])
    };
    for s in &secrets {
        let mut row = vec![
            s.id.clone(),
            if s.keys.is_empty() {
                "<none>".into()
            } else {
                s.keys.join(",")
            },
        ];
        if ctx.out.is_wide() {
            row.push(if s.encrypted_at_rest {
                "encrypted".into()
            } else {
                "plaintext".into()
            });
        }
        row.push(if s.updated_at == 0 {
            "—".into()
        } else {
            format!("{} (server clock)", s.updated_at)
        });
        if ctx.out.is_wide() {
            row.push(output::opt_str(s.description.as_deref()));
        }
        table.row(row);
    }
    table.print();
    Ok(())
}

/// `get jobs [-d DEPLOYMENT] [ID...]` — the image builds and host updates this
/// LB has run, newest first.
fn get_jobs(ctx: &Ctx, names: &[String], deployment: Option<&str>) -> Result<()> {
    let raw = match (names, deployment) {
        // A named job is looked up directly, so `get job job-abc` works without
        // knowing which deployment it belonged to.
        ([id], _) => ctx.client.get_job(id)?,
        ([], Some(d)) => ctx.client.deployment_jobs(d)?,
        ([], None) => ctx.client.list_jobs()?,
        _ => bail!("get job takes at most one job id; use -d to scope by deployment"),
    };

    let jobs: Vec<JobRecord> = match &raw {
        Value::Array(items) => items
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<_, _>>()
            .context("parsing the job list")?,
        other => vec![serde_json::from_value(other.clone()).context("parsing the job")?],
    };

    if ctx.out.is_machine() {
        let refs: Vec<String> = jobs.iter().map(|j| format!("job/{}", j.id)).collect();
        return output::emit(&raw, ctx.out, &refs);
    }

    if jobs.is_empty() {
        println!(
            "No jobs yet. (`serverctl set build <deployment> --repo <url>` records where an \
             image comes from; `serverctl set update <deployment> --workdir <dir> --command \
             '<cmd>'` records how a static one is updated.)"
        );
        return Ok(());
    }

    // A single named job is worth spelling out — its log is the reason somebody
    // asked for it by id.
    if let ([_], Some(record)) = (names, jobs.first())
        && jobs.len() == 1
    {
        return describe_job(record);
    }

    // TARGET and RESULT mean different things per kind (a ref and an image for a
    // build; a directory and a command count for an update), which is why the
    // KIND column is there.
    let mut table = Table::new([
        "JOB", "DEPLOYMENT", "KIND", "STATUS", "TARGET", "RESULT", "TOOK",
    ]);
    // Jobs are timestamped on the server's clock, so measure elapsed time
    // against the newest record rather than this machine's idea of now.
    let now = jobs
        .iter()
        .map(|j| j.finished_at.unwrap_or(j.started_at))
        .max()
        .unwrap_or(0);
    for j in &jobs {
        table.row([
            j.id.clone(),
            j.deployment.clone(),
            j.kind.clone(),
            j.status.clone(),
            j.target_summary(),
            j.result_summary(),
            output::duration(j.elapsed_secs(now)),
        ]);
    }
    table.print();
    Ok(())
}

fn describe_job(j: &JobRecord) -> Result<()> {
    output::top_field("Job", &j.id);
    output::top_field("Deployment", &j.deployment);
    output::top_field("Kind", &j.kind);

    output::section("Source");
    if j.is_update() {
        output::field("Working dir", output::opt_str(j.working_dir.as_deref()));
        output::field(
            "Commands",
            match (j.commands_run, j.commands_total) {
                (Some(run), Some(total)) => format!("{run} of {total} completed"),
                (_, Some(total)) => format!("{total}"),
                _ => "—".into(),
            },
        );
    } else if j.is_pull() {
        output::field("Store", output::opt_str(j.store.as_deref()));
        output::field("Ref", output::opt_str(j.artifact_ref.as_deref()));
        // The whole point of a pull: a tag can move, so the digest is what
        // actually says which bytes the pool is running.
        output::field("Digest", output::opt_str(j.digest.as_deref()));
        output::field(
            "Transferred",
            match (j.bytes, j.reused) {
                (Some(0), true) => "nothing — the image was already on the host".to_string(),
                (Some(n), _) => output::bytes(n),
                (None, _) => "—".into(),
            },
        );
    } else {
        output::field("Repo", &j.repo);
        output::field("Ref", j.git_ref.as_deref().unwrap_or("(default branch)"));
        output::field("Commit", output::opt_str(j.commit.as_deref()));
        output::field("Dockerfile", output::opt_str(j.dockerfile.as_deref()));
    }

    output::section("Result");
    output::field("Status", &j.status);
    if j.is_update() {
        output::field(
            "Upstreams",
            match j.verified {
                Some(true) => "healthy after the update",
                Some(false) => "did NOT come back — the host has already been changed",
                None if j.is_running() => "not checked yet",
                // No verdict on a finished job means it never got that far —
                // except when it succeeded, which can only mean the check is off.
                None if j.succeeded() => "not checked (verify_timeout_secs is 0)",
                None => "not checked — the job failed before that point",
            },
        );
    } else {
        output::field("Image", output::opt_str(j.image.as_deref()));
        output::field(
            "Rolled out",
            if j.rolled_out {
                "yes — vm.image updated, pool recycled"
            } else if j.is_running() {
                "not yet"
            } else {
                "no"
            },
        );
    }
    output::field(
        "Took",
        output::duration(j.elapsed_secs(j.finished_at.unwrap_or(j.started_at))),
    );
    if let Some(e) = &j.error {
        output::field("Error", e);
    }

    output::section("Log");
    if j.log.is_empty() {
        println!("  (no output yet)");
    }
    for line in &j.log {
        println!("  {line}");
    }
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

        // The static counterpart of a managed deployment's "Build source": what
        // `serverctl update` would run, and where.
        if let Some(update) = &d.spec.update {
            output::section("Update (on the app-lb host)");
            output::field("Working dir", &update.working_dir);
            output::field(
                "Verify",
                match update.verify_timeout_secs {
                    Some(0) => "off — a successful job only means the commands exited 0".into(),
                    Some(s) => format!("re-probe the upstreams, up to {}", output::duration(s)),
                    None => "re-probe the upstreams, up to 1m".into(),
                },
            );
            if let Some(t) = update.timeout_secs {
                output::field("Command timeout", output::duration(t));
            }
            if let Some(env) = &update.env {
                let keys: Vec<&str> = env.keys().map(String::as_str).collect();
                if !keys.is_empty() {
                    output::field("Env", keys.join(", "));
                }
            }
            if !update.env_from.is_empty() {
                let refs: Vec<String> = update.env_from.iter().map(|e| e.render()).collect();
                output::field("Env from secrets", refs.join(", "));
            }
            if let Some(auth) = &update.auth {
                output::field("Git credential", format!("secret {}", auth.render()));
            }
            println!("  Commands:");
            for (i, c) in update.commands.iter().enumerate() {
                println!("    {}. {c}", i + 1);
            }
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

        if let Some(build) = &d.spec.build {
            output::section("Build source");
            output::field("Repo", &build.repo);
            output::field("Ref", build.git_ref.as_deref().unwrap_or("(default branch)"));
            output::field(
                "Dockerfile",
                build
                    .dockerfile
                    .as_deref()
                    .unwrap_or("(found in the checkout)"),
            );
            if let Some(c) = &build.context {
                output::field("Context", c);
            }
            output::field(
                "Image name",
                build
                    .image_name
                    .as_deref()
                    .unwrap_or("(the deployment id) + commit"),
            );
            if let Some(mb) = build.image_size_mb {
                output::field("Rootfs size", format!("{mb} MB"));
            }
            // A reference, not a value: the credential itself is only ever
            // resolved server-side, when a build runs.
            output::field(
                "Credential",
                match &build.auth {
                    Some(a) => format!("secret {}", a.render()),
                    None => "none (public repo, or host ssh keys)".to_string(),
                },
            );
        }

        // The other image source. Never both — app-lb refuses a spec holding
        // one of each — so these two sections cannot appear together.
        if let Some(artifact) = &d.spec.artifact {
            output::section("Artifact source");
            output::field("Store", &artifact.store);
            let remote = artifact.store.starts_with("http://")
                || artifact.store.starts_with("https://");
            output::field(
                "Transport",
                if remote {
                    "streamed over HTTP, digest verified on arrival"
                } else {
                    "materialized locally by `art` (hole-aware)"
                },
            );
            // Which of the two it is decides whether the deployment follows a
            // moving tag or is pinned, and that is the thing worth knowing.
            output::field(
                "Ref",
                match artifact.artifact_ref.len() == 64
                    && artifact.artifact_ref.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    true => format!("{} (a digest — pinned)", artifact.artifact_ref),
                    false => format!("{} (a tag — resolved at pull time)", artifact.artifact_ref),
                },
            );
            output::field(
                "Image name",
                artifact
                    .image_name
                    .as_deref()
                    .unwrap_or("(the deployment id) + digest"),
            );
            output::field(
                "Grow to",
                match artifact.grow_gb {
                    Some(gb) => format!("{gb} GiB (sparse)"),
                    None => "(the image's stored size)".to_string(),
                },
            );
            output::field(
                "Credential",
                match (&artifact.auth, remote) {
                    (Some(a), true) => format!("secret {}", a.render()),
                    // Said rather than shown as configured: the server logs it
                    // as unused on every pull, and this is where somebody would
                    // look first.
                    (Some(a), false) => {
                        format!("secret {} — UNUSED, a local store has no API key", a.render())
                    }
                    (None, _) => "none (an ungated store)".to_string(),
                },
            );
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

    if let Some(auth) = &d.spec.auth {
        output::section("Sign-in gate");
        output::field(
            "Provider",
            if auth.provider.is_empty() {
                "google"
            } else {
                &auth.provider
            },
        );
        output::field("Client id", &auth.client_id);
        output::field("Client secret", format!("secret {}", auth.client_secret.render()));
        output::field("Who may enter", auth.allow_summary());
        if !auth.public_paths.is_empty() {
            output::field("Public paths", auth.public_paths.join(", "));
        }
        // The single most common reason a gate doesn't work is that this exact
        // URL is not registered with the provider, so print it rather than
        // leaving it to be assembled by hand.
        let host = d
            .spec
            .routes
            .iter()
            .find_map(|r| r.host.clone())
            .unwrap_or_else(|| "<this deployment's hostname>".into());
        output::field("Redirect URI", match &auth.redirect_url {
            Some(u) => format!("{u} (overridden)"),
            None => auth.callback_url(&host),
        });
        output::field("Session lifetime", output::duration(auth.session_ttl_secs));
        output::field(
            "Identity headers",
            if auth.forward_identity {
                "x-auth-request-email, -user, -name"
            } else {
                "not forwarded"
            },
        );
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
