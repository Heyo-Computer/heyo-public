//! Destructive operations exist only here, never over HTTP.
//!
//! `gc`, `rm`, `untag` and `heyvm sparsify --in-place` all modify or delete
//! data that a remote caller has no business reaching. When the daemon arrives
//! it will expose content, not lifecycle.
//!
//! Output is plain text by default and JSON under `--json`, so the same command
//! serves a human reading a terminal and heyvm parsing a subprocess.

use crate::config::{Config, parse_duration};
use crate::digest::Digest;
use crate::error::{Error, IoContext, Result};
use crate::gc::{GcPolicy, GcReport};
use crate::heyvm;
use crate::store::{BlobInfo, Materialize, Store};
use crate::sys::sparse::Shape;
use crate::tags::{Ref, TagName};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "art",
    version,
    about = "Content-addressed artifact store for ext4",
    long_about = "A content-addressed artifact store tuned for ext4 and wired into heyvm.\n\n\
                  ext4 cannot reflink, so deduplication is by hardlink and st_nlink is the \
                  reference count. Images are stored sparsely by punching out their zero runs, \
                  which is where the space saving comes from — ext4 base images are fully \
                  allocated on disk but mostly empty inside."
)]
pub struct Cli {
    /// Store root. Defaults to $ART_ROOT, then ~/.artifacts.
    #[arg(long, global = true, env = "ART_ROOT")]
    pub root: Option<PathBuf>,

    /// Refuse writes that would leave less than this many bytes free.
    #[arg(long, global = true)]
    pub min_free_bytes: Option<u64>,

    /// Emit JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the store directories.
    Init,
    /// Store a file (or `-` for stdin) and print its digest.
    Put {
        path: PathBuf,
        /// Scan for zero runs and store them as holes. Right for disk images,
        /// pointless for already-compressed data.
        #[arg(long)]
        squash: bool,
        /// Tag the result.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Write a blob's content to a file.
    Get {
        reference: String,
        #[arg(short, long)]
        output: PathBuf,
        /// Make a private writable copy instead of a hardlink.
        #[arg(long)]
        writable: bool,
    },
    /// Write a blob's content to stdout.
    Cat { reference: String },
    /// List store contents.
    Ls {
        #[command(flatten)]
        what: LsWhat,
    },
    /// Show one blob's size, allocation and link count.
    Stat { reference: String },
    /// Print a manifest as JSON.
    ///
    /// The read half of `put-manifest`, and the only way to see what a tag
    /// points *at* rather than what it resolves *to*. A consumer driving `art`
    /// as a subprocess uses this to find a manifest's entries without needing
    /// the store's HTTP daemon.
    Manifest { reference: String },
    /// Point a tag at a digest.
    Tag { name: String, reference: String },
    /// Remove a tag. The blob it named becomes collectable.
    Untag { name: String },
    /// Delete a blob outright, ignoring reachability.
    Rm {
        reference: String,
        /// Delete even while materializations exist.
        #[arg(long)]
        force: bool,
    },
    /// Re-hash blobs and confirm they still match their names.
    Verify {
        reference: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Report logical size, physical size and free space.
    Usage,
    /// Remove unreachable blobs.
    Gc {
        /// Report without deleting.
        #[arg(long)]
        dry_run: bool,
        /// Keep anything newer than this (e.g. 1h, 30m).
        #[arg(long, value_parser = parse_duration)]
        min_age: Option<Duration>,
    },
    /// Serve the store over HTTP.
    #[cfg(feature = "daemon")]
    Serve {
        /// Listen address. `0.0.0.0` inside a VM, so the host can reach it.
        #[arg(long, env = "ART_LISTEN", default_value = "127.0.0.1:8080")]
        listen: std::net::SocketAddr,
        /// Shared secret for `Authorization: Bearer` / `X-Api-Key`. Unset means
        /// every route is open; `/healthz` is open either way.
        #[arg(long, env = "ART_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// Refuse every mutating route.
        #[arg(long, env = "ART_READ_ONLY")]
        read_only: bool,
        /// Dashboard password. The dashboard is not served at all unless this
        /// is set — a store's tag names describe what you build.
        #[arg(long, env = "ART_ADMIN_PASSWORD", hide_env_values = true)]
        admin_password: Option<String>,
        /// Dashboard username.
        #[arg(long, env = "ART_ADMIN_USER", default_value = crate::config::DEFAULT_ADMIN_USER)]
        admin_user: String,
        /// Serve the dashboard with no login at all. For a listener already on
        /// a private network; conflicts with `--admin-password`.
        ///
        /// Boolish rather than clap's strict `true`/`false`, because the thing
        /// people actually write in a unit file or a compose file is `=1`, and
        /// a refusal to start is a poor answer to a correctly-expressed intent.
        #[arg(
            long,
            env = "ART_DASHBOARD_OPEN",
            value_parser = clap::builder::BoolishValueParser::new(),
            default_value = "false",
            default_missing_value = "true",
            num_args = 0..=1,
        )]
        dashboard_open: bool,
    },
    /// Dockerfiles that define a rootfs.
    #[command(subcommand)]
    Dockerfile(DockerfileCommand),
    /// heyvm integration.
    #[command(subcommand)]
    Heyvm(HeyvmCommand),
}

#[derive(Debug, Args)]
#[group(multiple = false)]
pub struct LsWhat {
    #[arg(long)]
    pub blobs: bool,
    #[arg(long)]
    pub tags: bool,
    #[arg(long)]
    pub manifests: bool,
}

#[derive(Debug, Subcommand)]
pub enum DockerfileCommand {
    /// Store a Dockerfile (and optionally its build context) as a manifest, and
    /// tag it.
    ///
    /// The manifest is a build *input*, not an image: nothing here is bootable.
    /// app-lb's `build.store` points at one of these and runs
    /// `heyvm mvm build` over it.
    Put {
        /// Path to the Dockerfile.
        path: PathBuf,
        /// Build context: a directory to pack, or an existing `.tar`/`.tar.gz`.
        /// Omit for a recipe that copies nothing in.
        #[arg(long)]
        context: Option<PathBuf>,
        /// Tag to point at the manifest.
        #[arg(long)]
        tag: Option<String>,
        /// Default name for the image this builds.
        #[arg(long)]
        image_name: Option<String>,
        /// Default rootfs size in megabytes, for `heyvm mvm build --size-mb`.
        #[arg(long)]
        size_mb: Option<u64>,
        /// Provenance note. Part of the manifest's address, so two pushes that
        /// differ only here are two manifests.
        #[arg(long)]
        source: Option<String>,
    },
    /// Show what a Dockerfile manifest holds.
    Show { reference: String },
    /// Write a Dockerfile manifest's entries into a directory.
    ///
    /// Produces `<dir>/Dockerfile` and, when the manifest has one,
    /// `<dir>/context.tar.gz`. Unpacking the archive is left to the caller —
    /// the rules for what an archive may contain belong to whoever runs it.
    Export { reference: String, dir: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum HeyvmCommand {
    /// Punch the zero runs out of heyvm's base images, in place.
    ///
    /// Images are fully allocated on disk but mostly empty inside, so this is
    /// where the space comes from. Content is unchanged and proven so by
    /// re-hashing.
    Sparsify {
        /// Images to process. Defaults to every *.ext4 in the image directory.
        names: Vec<String>,
        /// Report the saving without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the post-write re-read that proves content is unchanged.
        #[arg(long)]
        no_verify: bool,
    },
    /// Import heyvm base images into the store.
    Import {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Produce a writable rootfs from a stored image.
    Materialize {
        reference: String,
        dest: PathBuf,
        /// Extend the image to this many gigabytes. heyvm still runs
        /// `resize2fs` to make the guest filesystem use the room.
        #[arg(long)]
        grow_gb: Option<u64>,
    },
    /// Import a heyvm sync-bundle directory.
    BundleImport { dir: PathBuf },
    /// Write a stored bundle back out as a directory.
    BundleExport { reference: String, dir: PathBuf },
}

/// Exit codes. Distinguished so a caller can tell "you asked for the
/// impossible" from "the machine is out of room".
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_NO_SPACE: i32 = 3;

pub fn exit_code(e: &Error) -> i32 {
    match e {
        Error::NoSpace { .. } => EXIT_NO_SPACE,
        Error::Digest(_) | Error::TagName(_) => EXIT_USAGE,
        _ => EXIT_FAILURE,
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = Config::resolve(cli.root.clone(), cli.min_free_bytes, None)
        .map_err(|m| Error::Io {
            context: m,
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad configuration"),
        })?;
    let store = Store::open(&config)?;
    let json = cli.json;

    match cli.command {
        Command::Init => {
            if json {
                print_json(&serde_json::json!({"root": store.root()}));
            } else {
                println!("initialized {}", store.root().display());
            }
        }

        Command::Put { path, squash, tag } => {
            let shape = if squash { Shape::SQUASH } else { Shape::Dense };
            let info = if path.as_os_str() == "-" {
                store.insert_stream(std::io::stdin(), shape).await?
            } else {
                store.insert_path(&path, shape).await?
            };
            if let Some(t) = tag {
                store.set_tag(&TagName::parse(&t)?, &info.digest).await?;
            }
            if json {
                print_json(&blob_json(&info));
            } else {
                println!("{}", info.digest);
            }
        }

        Command::Get {
            reference,
            output,
            writable,
        } => {
            let d = store.resolve_blob(&parse_ref(&reference)?).await?;
            let how = if writable {
                Materialize::Writable {
                    shape: Shape::HoleAware,
                    mode: 0o644,
                }
            } else {
                Materialize::ReadOnly
            };
            let m = store.materialize(&d, &output, how).await?;
            if json {
                print_json(&serde_json::json!({
                    "path": m.path,
                    "digest": m.digest.as_str(),
                    "method": m.method.to_string(),
                    "bytesWritten": m.bytes_written,
                }));
            } else {
                println!(
                    "{} -> {} ({}, {} written)",
                    m.digest,
                    m.path.display(),
                    m.method,
                    human(m.bytes_written)
                );
            }
        }

        Command::Cat { reference } => {
            let d = store.resolve_blob(&parse_ref(&reference)?).await?;
            let mut f = store.open_blob(&d).await?;
            let mut out = std::io::stdout().lock();
            std::io::copy(&mut f, &mut out).ctx("write to stdout")?;
        }

        Command::Ls { what } => list(&store, &what, json).await?,

        Command::Stat { reference } => {
            let d = store.resolve_blob(&parse_ref(&reference)?).await?;
            let info = store.stat(&d).await?;
            if json {
                print_json(&blob_json(&info));
            } else {
                println!("digest     {}", info.digest);
                println!("size       {} ({})", info.size, human(info.size));
                println!("allocated  {} ({})", info.allocated, human(info.allocated));
                println!("links      {}", info.nlink);
                println!("outstanding materializations {}", info.nlink.saturating_sub(1));
            }
        }

        Command::Manifest { reference } => {
            let d = store.resolve(&parse_ref(&reference)?).await?;
            let m = store.get_manifest(&d).await?;
            if json {
                // The manifest itself, not a wrapper: a caller parsing this
                // wants the same shape `GET /manifests/{ref}` answers with, so
                // one consumer can be written against both transports.
                print_json(&serde_json::to_value(&m).expect("a Manifest is always serializable"));
            } else {
                println!("digest     {d}");
                println!("kind       {}", m.kind);
                for e in &m.entries {
                    println!("entry      {}  {}  {}", e.name, e.digest, human(e.size));
                }
                for (k, v) in &m.annotations {
                    println!("annotation {k}={v}");
                }
            }
        }

        Command::Tag { name, reference } => {
            let t = TagName::parse(&name)?;
            let d = store.resolve(&parse_ref(&reference)?).await?;
            store.set_tag(&t, &d).await?;
            if !json {
                println!("{t} -> {d}");
            }
        }

        Command::Untag { name } => {
            let t = TagName::parse(&name)?;
            if !store.remove_tag(&t).await? {
                return Err(Error::TagNotFound(name));
            }
            if !json {
                println!("removed tag {t}");
            }
        }

        Command::Rm { reference, force } => {
            let d = store.resolve_blob(&parse_ref(&reference)?).await?;
            let info = store.stat(&d).await?;
            if info.nlink > 1 && !force {
                return Err(Error::LinkLimit {
                    digest: d,
                    nlink: info.nlink,
                });
            }
            crate::sys::sparse::unlink_if_present(&store.blob_path(&d))?;
            if !json {
                println!("removed {d}");
            }
        }

        Command::Verify { reference, all } => {
            let targets: Vec<Digest> = if all {
                store.list_blobs().await?.into_iter().map(|b| b.digest).collect()
            } else {
                let r = reference.ok_or_else(|| Error::Io {
                    context: "verify needs a reference or --all".into(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing argument"),
                })?;
                vec![store.resolve_blob(&parse_ref(&r)?).await?]
            };
            let n = targets.len();
            for d in targets {
                store.verify(&d).await?;
                if !json {
                    println!("ok {d}");
                }
            }
            if json {
                print_json(&serde_json::json!({"verified": n}));
            }
        }

        Command::Usage => {
            let u = store.usage().await?;
            if json {
                print_json(&serde_json::json!({
                    "blobs": u.blobs,
                    "logical": u.logical,
                    "allocated": u.allocated,
                    "manifests": u.manifests,
                    "tags": u.tags,
                    "fsAvailable": u.fs_available,
                    "fsTotal": u.fs_total,
                }));
            } else {
                println!("blobs      {}", u.blobs);
                println!("manifests  {}", u.manifests);
                println!("tags       {}", u.tags);
                println!("logical    {}", human(u.logical));
                println!("allocated  {}", human(u.allocated));
                if u.allocated > 0 && u.logical > u.allocated {
                    println!(
                        "saved      {} ({:.1}x)",
                        human(u.logical - u.allocated),
                        u.logical as f64 / u.allocated as f64
                    );
                }
                println!(
                    "filesystem {} free of {}",
                    human(u.fs_available),
                    human(u.fs_total)
                );
            }
        }

        Command::Gc { dry_run, min_age } => {
            let policy = GcPolicy {
                min_age: min_age.unwrap_or(config.gc_min_age),
                dry_run,
            };
            let r = crate::gc::collect(&store, policy).await?;
            report_gc(&store, &r, json, dry_run);
        }

        #[cfg(feature = "daemon")]
        Command::Serve {
            listen,
            api_key,
            read_only,
            admin_password,
            admin_user,
            dashboard_open,
        } => {
            let dashboard =
                crate::config::DashboardAccess::resolve(admin_password, admin_user, dashboard_open)
                    .map_err(|m| Error::Io {
                        context: m,
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "bad configuration",
                        ),
                    })?;
            // `Store::open` above already created the layout, so the daemon
            // starts against a store that exists.
            crate::http::serve(
                &config,
                crate::http::ServeOptions {
                    addr: listen,
                    api_key,
                    read_only,
                    dashboard,
                },
            )
            .await?;
        }

        Command::Dockerfile(cmd) => run_dockerfile(&store, cmd, json).await?,

        Command::Heyvm(cmd) => run_heyvm(&store, &config, cmd, json).await?,
    }
    Ok(())
}

async fn run_dockerfile(store: &Store, cmd: DockerfileCommand, json: bool) -> Result<()> {
    use crate::dockerfile;

    match cmd {
        DockerfileCommand::Put {
            path,
            context,
            tag,
            image_name,
            size_mb,
            source,
        } => {
            // Resolved before anything is packed or stored: a tag the store
            // would refuse should not cost a full context pack first.
            let tag = tag.map(|t| TagName::parse(&t)).transpose()?;

            // A directory is packed into the store's own scratch, not `/tmp`:
            // a context can be gigabytes and `/tmp` is often a small tmpfs, and
            // packing beside the store keeps the insert on one filesystem.
            let packed = match &context {
                Some(c) if c.is_dir() => {
                    let dest = Scratch::new(
                        store
                            .tmp_dir()
                            .join(format!(".context.{}.tar.gz", std::process::id())),
                    );
                    let size = dockerfile::pack_context(c, dest.path())?;
                    if !json {
                        eprintln!("packed {} into {}", c.display(), human(size));
                    }
                    Some(dest)
                }
                _ => None,
            };
            let archive = match (&packed, &context) {
                (Some(p), _) => Some(p.path()),
                (None, Some(c)) => Some(c.as_path()),
                (None, None) => None,
            };

            let stored = dockerfile::put(
                store,
                &path,
                archive,
                tag,
                // `source` is left unset unless asked for. Defaulting it to the
                // Dockerfile's path would put the pushing machine's directory
                // layout into the manifest's address, so the same recipe pushed
                // from two checkouts would be two manifests that dedupe to one
                // blob and no further.
                &dockerfile::Options {
                    image_name,
                    size_mb,
                    source,
                },
            )
            .await?;

            if json {
                print_json(&serde_json::json!({
                    "manifest": stored.manifest.as_str(),
                    "dockerfile": blob_json(&stored.dockerfile),
                    "context": stored.context.as_ref().map(blob_json),
                    "tag": stored.tag.as_ref().map(TagName::as_str),
                }));
            } else {
                println!("manifest   {}", stored.manifest);
                println!(
                    "Dockerfile {}  {}",
                    stored.dockerfile.digest,
                    human(stored.dockerfile.size)
                );
                match &stored.context {
                    Some(c) => println!("context    {}  {}", c.digest, human(c.size)),
                    None => println!("context    (none)"),
                }
                if let Some(t) = &stored.tag {
                    println!("tag        {t}");
                }
            }
        }

        DockerfileCommand::Show { reference } => {
            let d = store.resolve(&parse_ref(&reference)?).await?;
            let m = store.get_manifest(&d).await?;
            if !dockerfile::is_dockerfile(&m) {
                return Err(Error::Io {
                    context: format!(
                        "{reference} is a {:?} manifest, not {:?}",
                        m.kind,
                        crate::manifest::KIND_DOCKERFILE
                    ),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "wrong manifest kind",
                    ),
                });
            }
            let df = dockerfile::dockerfile_entry(&m)?;
            let ctx = dockerfile::context_entry(&m);

            if json {
                print_json(&serde_json::json!({
                    "manifest": d.as_str(),
                    "kind": m.kind,
                    "dockerfile": {"digest": df.digest.as_str(), "size": df.size},
                    "context": ctx.map(|c| serde_json::json!({
                        "digest": c.digest.as_str(),
                        "size": c.size,
                    })),
                    "imageName": dockerfile::image_name(&m),
                    "sizeMb": dockerfile::size_mb(&m),
                    "annotations": m.annotations,
                }));
            } else {
                println!("manifest   {d}");
                println!("Dockerfile {}  {}", df.digest, human(df.size));
                match ctx {
                    Some(c) => println!("context    {}  {}", c.digest, human(c.size)),
                    None => println!("context    (none — this recipe copies nothing in)"),
                }
                if let Some(n) = dockerfile::image_name(&m) {
                    println!("image name {n}");
                }
                if let Some(mb) = dockerfile::size_mb(&m) {
                    println!("size       {mb} MB");
                }
            }
        }

        DockerfileCommand::Export { reference, dir } => {
            let e = dockerfile::export(store, &parse_ref(&reference)?, &dir).await?;
            if json {
                print_json(&serde_json::json!({
                    "manifest": e.manifest.as_str(),
                    "dockerfile": {
                        "path": e.dockerfile.path,
                        "digest": e.dockerfile.digest.as_str(),
                        "method": e.dockerfile.method.to_string(),
                        "bytesWritten": e.dockerfile.bytes_written,
                    },
                    "context": e.context.as_ref().map(|c| serde_json::json!({
                        "path": c.path,
                        "digest": c.digest.as_str(),
                        "method": c.method.to_string(),
                        "bytesWritten": c.bytes_written,
                    })),
                }));
            } else {
                println!("{}  ({})", e.dockerfile.path.display(), e.dockerfile.method);
                if let Some(c) = &e.context {
                    println!("{}  ({})", c.path.display(), c.method);
                }
            }
        }
    }
    Ok(())
}

/// A file removed on drop. For something staged on the way into the store,
/// where every early return between "packed" and "inserted" would otherwise
/// leave it behind in the store's scratch directory.
struct Scratch(PathBuf);

impl Scratch {
    fn new(path: PathBuf) -> Self {
        Scratch(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn list(store: &Store, what: &LsWhat, json: bool) -> Result<()> {
    // Tags are the default view: they are the names a human actually knows.
    if what.blobs {
        let blobs = store.list_blobs().await?;
        if json {
            let v: Vec<_> = blobs.iter().map(blob_json).collect();
            print_json(&serde_json::Value::Array(v));
        } else {
            for b in &blobs {
                println!(
                    "{}  {:>10}  {:>10}  links={}",
                    b.digest,
                    human(b.size),
                    human(b.allocated),
                    b.nlink
                );
            }
        }
    } else if what.manifests {
        let ds = store.list_manifests().await?;
        if json {
            let v: Vec<_> = ds.iter().map(|d| serde_json::json!(d.as_str())).collect();
            print_json(&serde_json::Value::Array(v));
        } else {
            for d in ds {
                let kind = store
                    .get_manifest(&d)
                    .await
                    .map(|m| m.kind)
                    .unwrap_or_else(|_| "?".into());
                println!("{d}  {kind}");
            }
        }
    } else {
        let tags = store.list_tags().await?;
        if json {
            let v: Vec<_> = tags
                .iter()
                .map(|(t, d)| serde_json::json!({"tag": t.as_str(), "digest": d.as_str()}))
                .collect();
            print_json(&serde_json::Value::Array(v));
        } else {
            for (t, d) in tags {
                println!("{t}\t{d}");
            }
        }
    }
    Ok(())
}

async fn run_heyvm(
    store: &Store,
    config: &Config,
    cmd: HeyvmCommand,
    json: bool,
) -> Result<()> {
    match cmd {
        HeyvmCommand::Sparsify {
            names,
            dry_run,
            no_verify,
        } => {
            let paths = resolve_image_paths(config, &names)?;
            let mut reports = Vec::new();
            let mut freed = 0u64;
            for p in paths {
                match heyvm::sparsify(&p, dry_run, !no_verify).await {
                    Ok(r) => {
                        freed += r.freed();
                        if !json {
                            println!(
                                "{:<28} {:>10} -> {:>10}  {} {}",
                                p.file_name().unwrap_or_default().to_string_lossy(),
                                human(r.allocated_before),
                                human(r.allocated_after),
                                if dry_run { "would free" } else { "freed" },
                                human(r.freed())
                            );
                        }
                        reports.push(r);
                    }
                    // One shared or unreadable image must not abort the sweep
                    // over the rest.
                    Err(e) => {
                        eprintln!("skipping {}: {e}", p.display());
                    }
                }
            }
            if json {
                let v: Vec<_> = reports
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "path": r.path,
                            "size": r.size,
                            "allocatedBefore": r.allocated_before,
                            "allocatedAfter": r.allocated_after,
                            "freed": r.freed(),
                            "digest": r.digest.as_str(),
                            "dryRun": r.dry_run,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({"images": v, "totalFreed": freed}));
            } else {
                println!(
                    "\n{} {} across {} image(s)",
                    if dry_run { "would free" } else { "freed" },
                    human(freed),
                    reports.len()
                );
            }
        }

        HeyvmCommand::Import { names, all } => {
            let paths = if all || names.is_empty() {
                heyvm::list_images(&config.heyvm_images_dir)?
            } else {
                resolve_image_paths(config, &names)?
            };
            let mut out = Vec::new();
            for p in paths {
                let imported = heyvm::import(store, &p, None).await?;
                if !json {
                    println!(
                        "{:<28} {}  {:>10} logical  {:>10} stored{}",
                        imported.name.as_str(),
                        &imported.blob.digest.as_str()[..12],
                        human(imported.blob.size),
                        human(imported.blob.allocated),
                        if imported.blob.deduped {
                            "  (deduplicated)"
                        } else {
                            ""
                        }
                    );
                }
                out.push(imported);
            }
            if json {
                let v: Vec<_> = out
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "name": i.name.as_str(),
                            "digest": i.blob.digest.as_str(),
                            "manifest": i.manifest.as_str(),
                            "size": i.blob.size,
                            "allocated": i.blob.allocated,
                            "deduped": i.blob.deduped,
                        })
                    })
                    .collect();
                print_json(&serde_json::Value::Array(v));
            }
        }

        HeyvmCommand::Materialize {
            reference,
            dest,
            grow_gb,
        } => {
            let r = parse_ref(&reference)?;
            let m = heyvm::materialize_rootfs(
                store,
                &r,
                &dest,
                heyvm::RootfsOptions {
                    grow_gb,
                    mode: 0o644,
                },
            )
            .await?;
            let final_size = crate::sys::space::stat_path(&dest)?.size;
            if json {
                print_json(&serde_json::json!({
                    "path": m.path,
                    "digest": m.digest.as_str(),
                    "method": m.method.to_string(),
                    "bytesWritten": m.bytes_written,
                    "size": final_size,
                }));
            } else {
                println!(
                    "{} -> {}\n{} written of {} nominal ({})",
                    m.digest,
                    m.path.display(),
                    human(m.bytes_written),
                    human(final_size),
                    m.method
                );
            }
        }

        HeyvmCommand::BundleImport { dir } => {
            let (digest, infos) = heyvm::bundle_import(store, &dir).await?;
            if json {
                print_json(&serde_json::json!({
                    "manifest": digest.as_str(),
                    "blobs": infos.iter().map(blob_json).collect::<Vec<_>>(),
                }));
            } else {
                println!("{digest}  ({} blobs)", infos.len());
            }
        }

        HeyvmCommand::BundleExport { reference, dir } => {
            let r = parse_ref(&reference)?;
            let mats = heyvm::bundle_export(store, &r, &dir).await?;
            if json {
                print_json(&serde_json::json!(
                    mats.iter()
                        .map(|m| serde_json::json!({
                            "path": m.path,
                            "method": m.method.to_string(),
                            "bytesWritten": m.bytes_written,
                        }))
                        .collect::<Vec<_>>()
                ));
            } else {
                for m in &mats {
                    println!("{}  ({})", m.path.display(), m.method);
                }
            }
        }
    }
    Ok(())
}

/// Turn bare image names into paths under heyvm's image directory, leaving
/// anything that looks like a path alone.
fn resolve_image_paths(config: &Config, names: &[String]) -> Result<Vec<PathBuf>> {
    if names.is_empty() {
        return heyvm::list_images(&config.heyvm_images_dir);
    }
    Ok(names
        .iter()
        .map(|n| {
            let p = PathBuf::from(n);
            if n.contains('/') || p.extension().is_some() {
                p
            } else {
                config.heyvm_images_dir.join(format!("{n}.ext4"))
            }
        })
        .collect())
}

fn report_gc(store: &Store, r: &GcReport, json: bool, dry_run: bool) {
    if json {
        print_json(&serde_json::json!({
            "scanned": r.scanned,
            "removed": r.removed.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            "bytesFreed": r.bytes_freed,
            "keptReachable": r.kept_reachable,
            "keptYoung": r.kept_young,
            "pinned": r.pinned.iter().map(|p| serde_json::json!({
                "digest": p.digest.as_str(),
                "size": p.size,
                "allocated": p.allocated,
                "links": p.links,
            })).collect::<Vec<_>>(),
            "manifestsRemoved": r.manifests_removed.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            "dryRun": dry_run,
        }));
        return;
    }
    println!("scanned      {}", r.scanned);
    println!("reachable    {}", r.kept_reachable);
    println!("within grace {}", r.kept_young);
    println!("pinned       {}", r.pinned.len());
    println!(
        "{}      {} ({})",
        if dry_run { "would remove" } else { "removed" },
        r.removed_count(),
        human(r.bytes_freed)
    );
    for p in &r.pinned {
        println!(
            "  pinned {} by {} materialization(s); find with:\n    {}",
            &p.digest.as_str()[..12],
            p.links,
            p.find_hint(store.root())
        );
    }
}

fn parse_ref(s: &str) -> Result<Ref> {
    Ok(Ref::parse(s)?)
}

fn blob_json(b: &BlobInfo) -> serde_json::Value {
    serde_json::json!({
        "digest": b.digest.as_str(),
        "size": b.size,
        "allocated": b.allocated,
        "nlink": b.nlink,
        "deduped": b.deduped,
    })
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).expect("serializable"));
}

/// Byte counts a human can read at a glance.
pub fn human(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn human_reads_naturally() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(21474836480), "20.0 GiB");
    }

    #[test]
    fn parses_a_representative_command_line() {
        let cli = Cli::try_parse_from([
            "art", "--json", "--root", "/srv/art", "gc", "--dry-run", "--min-age", "2h",
        ])
        .unwrap();
        assert!(cli.json);
        assert_eq!(cli.root, Some(PathBuf::from("/srv/art")));
        match cli.command {
            Command::Gc { dry_run, min_age } => {
                assert!(dry_run);
                assert_eq!(min_age, Some(Duration::from_secs(7200)));
            }
            other => panic!("expected Gc, got {other:?}"),
        }
    }

    #[test]
    fn heyvm_subcommands_parse() {
        let cli =
            Cli::try_parse_from(["art", "heyvm", "sparsify", "--dry-run", "debian-hermes"]).unwrap();
        match cli.command {
            Command::Heyvm(HeyvmCommand::Sparsify {
                names, dry_run, ..
            }) => {
                assert!(dry_run);
                assert_eq!(names, vec!["debian-hermes".to_string()]);
            }
            other => panic!("expected Sparsify, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "art",
            "heyvm",
            "materialize",
            "debian",
            "/tmp/rootfs.ext4",
            "--grow-gb",
            "20",
        ])
        .unwrap();
        match cli.command {
            Command::Heyvm(HeyvmCommand::Materialize { grow_gb, dest, .. }) => {
                assert_eq!(grow_gb, Some(20));
                assert_eq!(dest, PathBuf::from("/tmp/rootfs.ext4"));
            }
            other => panic!("expected Materialize, got {other:?}"),
        }
    }

    #[test]
    fn dockerfile_subcommands_parse() {
        let cli = Cli::try_parse_from([
            "art",
            "dockerfile",
            "put",
            "./Dockerfile",
            "--context",
            "./app",
            "--tag",
            "web-rootfs",
            "--size-mb",
            "4096",
        ])
        .unwrap();
        match cli.command {
            Command::Dockerfile(DockerfileCommand::Put {
                path,
                context,
                tag,
                size_mb,
                ..
            }) => {
                assert_eq!(path, PathBuf::from("./Dockerfile"));
                assert_eq!(context, Some(PathBuf::from("./app")));
                assert_eq!(tag.as_deref(), Some("web-rootfs"));
                assert_eq!(size_mb, Some(4096));
            }
            other => panic!("expected Dockerfile Put, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["art", "dockerfile", "export", "web-rootfs", "/tmp/b"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Command::Dockerfile(DockerfileCommand::Export { .. })
        ));
    }

    #[test]
    fn ls_flags_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["art", "ls", "--blobs", "--tags"]).is_err());
    }

    #[test]
    fn resolve_image_paths_expands_bare_names_only() {
        let cfg = Config {
            root: PathBuf::from("/srv/art"),
            min_free_bytes: 0,
            gc_min_age: Duration::ZERO,
            heyvm_images_dir: PathBuf::from("/home/u/.heyo/images/firecracker"),
        };
        let got = resolve_image_paths(
            &cfg,
            &[
                "debian".to_string(),
                "/abs/path.ext4".to_string(),
                "rel/img.ext4".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                PathBuf::from("/home/u/.heyo/images/firecracker/debian.ext4"),
                PathBuf::from("/abs/path.ext4"),
                PathBuf::from("rel/img.ext4"),
            ]
        );
    }

    #[test]
    fn exit_codes_distinguish_failure_modes() {
        assert_eq!(
            exit_code(&Error::NoSpace {
                needed: 1,
                available: 0,
                reserve: 0
            }),
            EXIT_NO_SPACE
        );
        assert_eq!(
            exit_code(&Error::TagNotFound("x".into())),
            EXIT_FAILURE
        );
        assert_eq!(
            exit_code(&Error::Digest(crate::digest::DigestError::BadChar)),
            EXIT_USAGE
        );
    }
}
