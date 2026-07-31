//! `serverctl token` — minting, listing and revoking app-tokens.
//!
//! An app-token is what a program authenticates with: scoped to particular
//! deployments, revocable without restarting app-lb, optionally expiring. Basic
//! auth stays for people and for whoever mints the first token.
//!
//! The secret is printed **once**, by `mint`, and cannot be recovered — app-lb
//! keeps only its hash. So `mint` writes the token to stdout on its own line and
//! everything else to stderr, which makes the useful thing pipeable:
//!
//! ```text
//! APP_LB_TOKEN=$(serverctl token mint agent --admin admin --deployment sb-1 -q)
//! ```

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::cmd::Ctx;
use crate::output::{self, Table};
use crate::{AdminScope, NewToken, TokenSummary};

#[derive(Subcommand, Debug)]
pub enum TokenCmd {
    /// Mint a token. Its secret is shown once and cannot be retrieved again.
    Mint(MintArgs),
    /// List live tokens. Never shows a secret.
    #[command(alias = "ls")]
    List,
    /// Show one token.
    Describe { id: String },
    /// Change a token's scope, name or expiry without changing its secret.
    Set(SetArgs),
    /// Revoke a token. Takes effect on the next request.
    #[command(alias = "rm")]
    Revoke(RevokeArgs),
}

#[derive(Args, Debug)]
pub struct MintArgs {
    /// What this token is for. Required — an unnamed token is one nobody dares
    /// revoke.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// What it may do on the admin API.
    #[arg(long, value_name = "SCOPE", default_value = "none")]
    pub admin: AdminScopeArg,

    /// A deployment this token may reach. Repeatable. Omit for none; use
    /// `--all-deployments` for every one.
    #[arg(long = "deployment", short = 'd', value_name = "ID")]
    pub deployments: Vec<String>,

    /// Scope to every deployment, present and future.
    #[arg(long, conflicts_with = "deployments")]
    pub all_deployments: bool,

    /// Expire after this many hours.
    #[arg(long, value_name = "HOURS")]
    pub expires_in: Option<u64>,

    /// Print only the token, for capturing into a variable.
    #[arg(long, short = 'q')]
    pub quiet: bool,
}

/// clap needs its own enum; the library's is not a `ValueEnum`.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum AdminScopeArg {
    /// No admin API access. For a token an application carries to get past its
    /// own deployment's gate.
    None,
    /// `/metrics` and the dashboard.
    View,
    /// Full CRUD, within the token's deployment scope.
    Admin,
}

impl From<AdminScopeArg> for AdminScope {
    fn from(a: AdminScopeArg) -> Self {
        match a {
            AdminScopeArg::None => Self::None,
            AdminScopeArg::View => Self::View,
            AdminScopeArg::Admin => Self::Admin,
        }
    }
}

#[derive(Args, Debug)]
pub struct SetArgs {
    #[arg(value_name = "ID")]
    pub id: String,
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    #[arg(long, value_name = "SCOPE")]
    pub admin: Option<AdminScopeArg>,
    /// Replace the deployment scope. Repeatable.
    #[arg(long = "deployment", short = 'd', value_name = "ID")]
    pub deployments: Vec<String>,
    #[arg(long, conflicts_with = "deployments")]
    pub all_deployments: bool,
    /// Remove the expiry, making the token permanent.
    #[arg(long)]
    pub never_expires: bool,
}

#[derive(Args, Debug)]
pub struct RevokeArgs {
    #[arg(value_name = "ID")]
    pub id: String,
    /// Skip the confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn run(ctx: &Ctx, cmd: &TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Mint(args) => mint(ctx, args),
        TokenCmd::List => list(ctx),
        TokenCmd::Describe { id } => describe(ctx, id),
        TokenCmd::Set(args) => set(ctx, args),
        TokenCmd::Revoke(args) => revoke(ctx, args),
    }
}

fn scope_of(deployments: &[String], all: bool) -> Vec<String> {
    if all {
        vec!["*".to_string()]
    } else {
        deployments.to_vec()
    }
}

fn mint(ctx: &Ctx, args: &MintArgs) -> Result<()> {
    let deployments = scope_of(&args.deployments, args.all_deployments);

    // A token with neither an admin scope nor a deployment can do nothing at
    // all. That is a safe default for a *forgotten* field and a mistake when
    // it is the whole request, so say so rather than mint a useless credential.
    if matches!(args.admin, AdminScopeArg::None) && deployments.is_empty() {
        bail!(
            "this token would have no access to anything — give it --admin, \
             or --deployment/-d, or --all-deployments"
        );
    }

    let mut req = NewToken::new(&args.name)
        .admin(args.admin.into())
        .for_deployments(deployments);
    if let Some(hours) = args.expires_in {
        req = req.expires_in(std::time::Duration::from_secs(hours * 3600));
    }

    let minted = ctx.client.raw().mint_token(&req)?;
    let secret = minted
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if args.quiet {
        println!("{secret}");
        return Ok(());
    }
    if ctx.out.is_machine() {
        return output::emit(&minted, ctx.out, &[]);
    }

    // The secret on stdout, the commentary on stderr: `$(serverctl token mint …)`
    // then captures the credential and nothing else, even without `-q`.
    eprintln!("Minted {:?}.", args.name);
    eprintln!("This is the only time the token is shown — app-lb stores only its hash.\n");
    println!("{secret}");
    eprintln!("\nUse it as: Authorization: Bearer <token>");
    Ok(())
}

fn scope_cell(t: &TokenSummary) -> String {
    if t.covers_fleet() {
        "*".to_string()
    } else if t.deployments.is_empty() {
        "—".to_string()
    } else {
        t.deployments.join(",")
    }
}

fn expiry_cell(t: &TokenSummary, now: u64) -> String {
    match t.expires_at {
        None => "never".to_string(),
        Some(at) if at <= now => "expired".to_string(),
        Some(at) => format!("in {}", output::duration(at - now)),
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    if ctx.out.is_machine() {
        let raw = ctx.client.raw().tokens()?;
        let names: Vec<String> = raw
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.get("id").and_then(Value::as_str))
                    .map(|id| format!("token/{id}"))
                    .collect()
            })
            .unwrap_or_default();
        return output::emit(&raw, ctx.out, &names);
    }

    let tokens = ctx.client.tokens()?;
    if tokens.is_empty() {
        println!("No app-tokens.");
        return Ok(());
    }
    let now = now_secs();
    let mut table = Table::new(["ID", "NAME", "ADMIN", "DEPLOYMENTS", "LAST USED", "EXPIRES"]);
    for t in &tokens {
        table.row(&[
            t.id.clone(),
            t.name.clone(),
            t.admin.to_string(),
            scope_cell(t),
            // "never" and "not since the store was last written" are
            // indistinguishable here; the stamp is flushed opportunistically.
            t.last_used_at
                .map(|u| format!("{} ago", output::duration(now.saturating_sub(u))))
                .unwrap_or_else(|| "—".into()),
            expiry_cell(t, now),
        ]);
    }
    table.print();
    Ok(())
}

fn describe(ctx: &Ctx, id: &str) -> Result<()> {
    if ctx.out.is_machine() {
        return output::emit(&ctx.client.raw().token(id)?, ctx.out, &[format!("token/{id}")]);
    }
    let t = ctx.client.token(id)?;
    let now = now_secs();
    output::section("Token");
    output::field("ID", &t.id);
    output::field("Name", &t.name);
    output::field("Admin API", t.admin.to_string());
    output::field(
        "Deployments",
        if t.covers_fleet() {
            "all (*)".to_string()
        } else if t.deployments.is_empty() {
            "none".to_string()
        } else {
            t.deployments.join(", ")
        },
    );
    output::field(
        "Created",
        format!("{} ago", output::duration(now.saturating_sub(t.created_at))),
    );
    output::field(
        "Last used",
        t.last_used_at
            .map(|u| format!("{} ago", output::duration(now.saturating_sub(u))))
            .unwrap_or_else(|| "never, or not since the store was last written".into()),
    );
    output::field("Expires", expiry_cell(&t, now));
    if !t.covers_fleet() && t.admin == AdminScope::Admin {
        println!(
            "\nScoped to specific deployments, so the fleet-wide routes — creating\n\
             deployments, the secret store, minting tokens — are refused."
        );
    }
    Ok(())
}

fn set(ctx: &Ctx, args: &SetArgs) -> Result<()> {
    let mut patch = json!({});
    if let Some(name) = &args.name {
        patch["name"] = json!(name);
    }
    if let Some(admin) = args.admin {
        patch["admin"] = json!(AdminScope::from(admin));
    }
    if args.all_deployments || !args.deployments.is_empty() {
        patch["deployments"] = json!(scope_of(&args.deployments, args.all_deployments));
    }
    if args.never_expires {
        // Explicit null clears it; an absent key would leave it alone.
        patch["expires_at"] = Value::Null;
    }
    if patch.as_object().is_none_or(|o| o.is_empty()) {
        bail!("nothing to change — pass --name, --admin, --deployment/--all-deployments or --never-expires");
    }

    let updated = ctx.client.raw().patch_token(&args.id, &patch)?;
    if ctx.out.is_machine() {
        return output::emit(&updated, ctx.out, &[format!("token/{}", args.id)]);
    }
    println!("token/{} updated (its secret is unchanged)", args.id);
    Ok(())
}

fn revoke(ctx: &Ctx, args: &RevokeArgs) -> Result<()> {
    if !args.yes {
        let t = ctx.client.token(&args.id)?;
        crate::cmd::write::confirm(&format!(
            "Revoke {:?} ({})? Anything using it stops working on its next request.",
            t.name, t.id
        ))?;
    }
    ctx.client.revoke_token(&args.id)?;
    println!("token/{} revoked", args.id);
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_deployments_beats_an_empty_list() {
        assert_eq!(scope_of(&[], true), ["*"]);
        assert_eq!(scope_of(&["a".into()], false), ["a"]);
        assert!(scope_of(&[], false).is_empty());
    }

    fn token(deployments: &[&str], expires_at: Option<u64>) -> TokenSummary {
        TokenSummary {
            id: "abc".into(),
            name: "t".into(),
            admin: AdminScope::Admin,
            deployments: deployments.iter().map(|s| s.to_string()).collect(),
            created_at: 0,
            expires_at,
            last_used_at: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn the_scope_cell_distinguishes_all_from_none() {
        assert_eq!(scope_cell(&token(&["*"], None)), "*");
        assert_eq!(scope_cell(&token(&[], None)), "—");
        assert_eq!(scope_cell(&token(&["a", "b"], None)), "a,b");
    }

    #[test]
    fn an_expired_token_is_labelled_rather_than_shown_as_a_negative_duration() {
        assert_eq!(expiry_cell(&token(&["*"], None), 100), "never");
        assert_eq!(expiry_cell(&token(&["*"], Some(50)), 100), "expired");
        assert_eq!(expiry_cell(&token(&["*"], Some(100)), 100), "expired");
        assert!(expiry_cell(&token(&["*"], Some(3700)), 100).starts_with("in "));
    }
}
