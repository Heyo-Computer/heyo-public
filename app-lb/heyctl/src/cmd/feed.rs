//! `heyctl feed` — the per-namespace event feed.
//!
//! app-lb keeps a bounded ring of events per namespace: deployments that opted
//! in (`feed.announce`, `feed.issues` in their spec) publish their lifecycle
//! and their operational trouble there, and the same entries are what the RSS
//! document carries. This command is the operator's view of it:
//!
//! ```text
//! heyctl feed                 # which namespaces have events
//! heyctl feed team-a          # the events, newest first
//! heyctl feed team-a --xml    # the RSS document itself, verbatim
//! ```
//!
//! The feed is in memory on the server; a restart empties it. The durable
//! record is `heyctl get jobs` and app-obs — this is the subscription view.

use anyhow::Result;
use clap::Args;

use crate::cmd::Ctx;
use crate::output::{self, Table};
use crate::types::FeedEvent;

#[derive(Args, Debug)]
pub struct FeedArgs {
    /// The namespace whose feed to show. Omit to list the namespaces that
    /// have events.
    #[arg(value_name = "NAMESPACE")]
    pub namespace: Option<String>,

    /// Print the RSS document itself — what a feed reader would fetch.
    #[arg(long, conflicts_with = "limit")]
    pub xml: bool,

    /// Show at most this many events.
    #[arg(long, short = 'n', value_name = "N")]
    pub limit: Option<usize>,
}

pub fn run(ctx: &Ctx, args: &FeedArgs) -> Result<()> {
    match &args.namespace {
        None => index(ctx),
        Some(ns) if args.xml => {
            // Verbatim, stdout only: `heyctl feed team-a --xml > feed.xml`
            // must produce exactly what the server serves.
            print!("{}", ctx.client.feed_rss(ns)?);
            Ok(())
        }
        Some(ns) => events(ctx, ns, args.limit),
    }
}

fn index(ctx: &Ctx) -> Result<()> {
    if ctx.out.is_machine() {
        let raw = ctx.client.raw().feeds()?;
        let names: Vec<String> = raw
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.get("namespace").and_then(serde_json::Value::as_str))
                    .map(|ns| format!("feed/{ns}"))
                    .collect()
            })
            .unwrap_or_default();
        return output::emit(&raw, ctx.out, &names);
    }

    let feeds = ctx.client.feeds()?;
    if feeds.is_empty() {
        println!("No feed events. Deployments publish only when their spec opts in");
        println!("(\"feed\": {{\"announce\": true, \"issues\": true}}).");
        return Ok(());
    }
    let mut table = Table::new(["NAMESPACE", "EVENTS"]);
    for f in &feeds {
        table.row(&[f.namespace.clone(), f.events.to_string()]);
    }
    table.print();
    Ok(())
}

fn events(ctx: &Ctx, namespace: &str, limit: Option<usize>) -> Result<()> {
    if ctx.out.is_machine() {
        let raw = ctx.client.raw().feed_events(namespace)?;
        let names: Vec<String> = raw
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("id").and_then(serde_json::Value::as_u64))
                    .map(|id| format!("feed/{namespace}/{id}"))
                    .collect()
            })
            .unwrap_or_default();
        return output::emit(&raw, ctx.out, &names);
    }

    let events = ctx.client.feed_events(namespace)?;
    if events.is_empty() {
        println!("No events in namespace {namespace:?}.");
        return Ok(());
    }
    let now = now_secs();
    let mut table = Table::new(["WHEN", "KIND", "DEPLOYMENT", "WHAT"]);
    for e in events.iter().take(limit.unwrap_or(usize::MAX)) {
        table.row(&[
            format!("{} ago", output::duration(now.saturating_sub(e.last_ts))),
            e.kind.clone(),
            e.deployment.clone(),
            what_cell(e),
        ]);
    }
    table.print();
    Ok(())
}

/// The title, with the detail and any fold count behind it — one line per
/// event, however noisy the source was.
fn what_cell(e: &FeedEvent) -> String {
    let mut out = e.title.clone();
    if !e.detail.is_empty() {
        out.push_str(" — ");
        out.push_str(&e.detail);
    }
    if e.count > 1 {
        out.push_str(&format!(" (×{})", e.count));
    }
    out
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

    fn event(title: &str, detail: &str, count: u64) -> FeedEvent {
        FeedEvent {
            title: title.into(),
            detail: detail.into(),
            count,
            ..Default::default()
        }
    }

    #[test]
    fn the_what_cell_folds_title_detail_and_count_into_one_line() {
        assert_eq!(what_cell(&event("web deployed", "", 1)), "web deployed");
        assert_eq!(
            what_cell(&event("web: VM failed to boot", "gave up after 300s", 1)),
            "web: VM failed to boot — gave up after 300s"
        );
        assert_eq!(
            what_cell(&event("web: cold start timed out", "a request waited 120s", 7)),
            "web: cold start timed out — a request waited 120s (×7)"
        );
    }
}
