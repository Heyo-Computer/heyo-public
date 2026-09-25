//! Rust SDK for the Heyo cloud sandbox API.
//!
//! Mirrors `sdk-ts/` (`@heyocomputer/sdk`). The public surface is centered
//! on two handles:
//!
//! - [`Sandbox`] — VM lifecycle, with `.commands()` and `.files()` sub-clients
//!   and an interactive WebSocket [`ShellSession`].
//! - [`HeyoClient`] — the underlying HTTP transport. Built from
//!   [`HeyoClientOptions`] (or constructed implicitly by the high-level
//!   `create`/`connect` helpers).
//!
//! ```no_run
//! use heyo_sdk::{Sandbox, SandboxCreateOptions, HeyoClientOptions};
//!
//! # async fn run() -> Result<(), heyo_sdk::HeyoError> {
//! let sandbox = Sandbox::create(
//!     SandboxCreateOptions { image: Some("ubuntu:24.04".into()), ..Default::default() },
//!     HeyoClientOptions::default(),
//! ).await?;
//! let out = sandbox.commands().run("echo hi", Default::default()).await?;
//! assert_eq!(out.stdout.trim(), "hi");
//! sandbox.kill().await?;
//! # Ok(()) }
//! ```

mod archive;
mod client;
mod commands;
mod daemons;
mod deployment_shell;
mod errors;
mod files;
mod namespaces;
mod networks;
mod p2p;
mod proxy;
mod sandbox;
mod shell;
mod transfer;
mod types;
#[cfg(unix)]
mod uds;
pub mod daemon;

pub use archive::{archive_dir, ArchiveDirOptions, ArchiveResult};
pub use client::{HeyoClient, HeyoClientOptions, RequestOptions, DEFAULT_LOCAL_BASE_URL};
pub use commands::Commands;
pub use daemon::{
    file_stream, BindRequest, Daemon, DaemonCreateRequest, DaemonCreated, DaemonMount, DiskPart,
    DiskPartKind, HostUsage, ImageInfo, ImageUploadOptions, InactivePage, InactiveSandbox, LogEntry,
    LogsQuery, ProxyBind, ProxyDeployment, PurgeFailure, PurgeOutcome, PurgeParts, SandboxDisks,
    SandboxLogs, SandboxUsage, StorageInventory, SystemUsage, TreeInfo, UploadStream, UsageSnapshot,
};
pub use daemons::{DaemonInfo, DaemonStatus, Daemons};
pub use deployment_shell::{DeploymentShell, DeploymentShellEvent, DeploymentShellOptions};
pub use errors::HeyoError;
pub use files::{FileContent, FileOptions, Files};
pub use namespaces::{
    DeploymentExecOptions, DeploymentExecResult, DeploymentSpec, DeploymentStatus,
    DeploymentSummary, DeploymentVm, Deployments, HealthCheck, Namespace, NamespaceCreateOptions,
    NamespaceInfo, NamespaceScope, RouteRule, ScalingPolicy, VmSpec,
    IngressInfo, SecretEnv, SecretPatch, SecretSpec, SecretSummary, Secrets, WorkspaceArchive,
};
pub use networks::{
    Network, NetworkCreateOptions, NetworkInfo, NetworkMember, NetworkMemberKind,
    NetworkMemberRegistration, NetworkService, NetworkUpdateOptions, ServiceRoute,
};
pub use p2p::P2pTunnel;
pub use sandbox::Sandbox;
pub use shell::{ShellEvent, ShellOptions, ShellReconnectOptions, ShellSession};
pub use transfer::{ReceiveOptions, Transfer, TransferReceiveStatus, TransferStatus};
pub use types::{
    BoundUrl, CommandResult, CommandRunOptions, PublicImage, SandboxCreateOptions,
    SandboxDriver, SandboxInfo, SandboxRegion, SandboxSize, SandboxStatus,
};
