mod agent;
mod agents;
mod cli;
mod codegraph_watch;
mod config;
mod drivers;
mod exec;
mod hooks;
mod host;
mod init;
mod metrics;
mod plan;
mod plugins;
mod prompts;
mod review;
mod run;
mod session;
mod skills;
mod spec_from_followups;
mod specs_paths;
mod tasks;
mod test;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_force_exit_supervisor();
    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Init(args) => init::init(args),
        cli::Command::Run(args) => run::run(args).await.map(|_| ()),
        cli::Command::Plan(args) => plan::plan(args).await.map(|_| ()),
        cli::Command::Review(args) => review::review(args).await.map(|_| ()),
        cli::Command::Test(args) => test::test(args).await,
        cli::Command::Exec(args) => exec::exec(args).await,
        cli::Command::History(args) => exec::print_history(args),
        cli::Command::Spec(args) => dispatch_spec(args),
        cli::Command::SpecFromFollowups(args) => {
            spec_from_followups::spec_from_followups(args).await
        }
        cli::Command::Task(args) => tasks::dispatch(args),
        cli::Command::AddPlugin(args) => plugins::add_plugin(args),
        cli::Command::ReinstallPlugin(args) => dispatch_reinstall_plugin(args),
        cli::Command::Plugins => plugins::list_installed(),
        cli::Command::Hooks(args) => dispatch_hooks(args),
        cli::Command::Config(args) => dispatch_config(args),
        cli::Command::External(args) => plugins::exec_external(&args),
    }
}

fn dispatch_spec(args: cli::SpecArgs) -> anyhow::Result<()> {
    let (target, phase) = match args.command {
        cli::SpecCommand::Complete(t) => (t, exec::Phase::Done),
        cli::SpecCommand::Cancel(t) => (t, exec::Phase::Cancelled),
    };
    exec::mark_spec_phase(target.cwd.as_deref(), &target.spec, phase)
}

fn dispatch_config(args: cli::ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        cli::ConfigCommand::Show => config::cli_show(),
        cli::ConfigCommand::Edit => config::cli_edit(),
    }
}

fn dispatch_reinstall_plugin(args: cli::ReinstallPluginArgs) -> anyhow::Result<()> {
    match (args.all, args.name.as_deref()) {
        (true, Some(_)) => {
            anyhow::bail!("--all and a positional plugin name are mutually exclusive")
        }
        (true, None) => plugins::reinstall_all(),
        (false, Some(name)) => plugins::reinstall_plugin(name),
        (false, None) => anyhow::bail!(
            "missing plugin name. Pass `printer reinstall-plugin <name>` or `--all`"
        ),
    }
}

fn dispatch_hooks(args: cli::HooksArgs) -> anyhow::Result<()> {
    match args.command {
        cli::HooksCommand::List(a) => {
            let set = hooks::HookSet::load_installed()?;
            set.print_list(a.event.as_deref())
        }
    }
}

/// Spawn a background task that watches Ctrl-C. The first press is just a
/// hint (the per-turn handler is expected to do the graceful shutdown). The
/// second press calls `std::process::exit(130)` so the user can always force
/// the CLI to quit, even if a child agent or some cleanup path is wedged.
fn install_force_exit_supervisor() {
    tokio::spawn(async {
        let mut count: u32 = 0;
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            count += 1;
            if count == 1 {
                eprintln!("\n[printer] Ctrl-C received; cleaning up — press again to force quit");
            } else {
                eprintln!("\n[printer] forced exit");
                std::process::exit(130);
            }
        }
    });
}
