//! The command line, which is almost nothing.
//!
//! app-lb is configured entirely by `APP_LB_*` environment variables, so the
//! only arguments a person should ever type are `--version` and `--help`. That
//! is exactly why parsing them matters: before this, *any* argument was ignored
//! and the server started. `app-lb --version` on a host that already runs one
//! started a second instance with an empty state file, and its startup
//! adoption treated every sandbox the real instance owned as an orphan from a
//! previous run. So an unknown argument is now a refusal, never a server.
//!
//! The internal helper entry points (`--forwarding-worker`,
//! `--apply-host-update`, `--bootstrap-host-update`) are dispatched in `main`
//! before this runs and are deliberately not listed in `--help`.

/// What the arguments ask for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// No arguments: run the load balancer.
    Serve,
    Version,
    Help,
    /// Anything else. Carries the first argument that was not understood.
    Invalid(String),
}

/// Decide from `args`, excluding the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Command {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Command::Serve;
    };
    let command = match first.as_str() {
        "--version" | "-V" | "version" => Command::Version,
        "--help" | "-h" | "help" => Command::Help,
        _ => return Command::Invalid(first),
    };
    match args.next() {
        None => command,
        Some(extra) => Command::Invalid(extra),
    }
}

/// `app-lb <version> (<revision>)`, the revision being the build's git SHA
/// when `HEYO_BUILD_GIT_SHA` was set at build time.
pub fn version() -> String {
    format!(
        "app-lb {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("APP_LB_BUILD_REVISION")
    )
}

pub const HELP: &str = "\
app-lb — an application load balancer for heyvm sandboxes

Usage: app-lb [--version | --help]

With no arguments, app-lb runs in the foreground. It is configured entirely by
APP_LB_* environment variables; see the README for the full list.

Only one app-lb may manage a given heyvm daemon. A second one refuses to start
rather than adopt, or kill, sandboxes that belong to the first.

Options:
  -V, --version   Print the version and exit
  -h, --help      Print this help and exit
";

/// Run a non-`Serve` command. Returns the process exit code.
pub fn run(command: &Command) -> i32 {
    match command {
        Command::Serve => unreachable!("the caller starts the server"),
        Command::Version => {
            println!("{}", version());
            0
        }
        Command::Help => {
            print!("{HELP}");
            0
        }
        Command::Invalid(arg) => {
            eprintln!(
                "app-lb: unrecognized argument {arg:?}; nothing was started.\n\
                 app-lb takes no arguments besides --version and --help — it is \
                 configured by APP_LB_* environment variables."
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Command {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_serves() {
        assert_eq!(parse_strs(&[]), Command::Serve);
    }

    #[test]
    fn version_and_help_are_recognised_in_every_spelling() {
        for a in ["--version", "-V", "version"] {
            assert_eq!(parse_strs(&[a]), Command::Version, "{a}");
        }
        for a in ["--help", "-h", "help"] {
            assert_eq!(parse_strs(&[a]), Command::Help, "{a}");
        }
    }

    #[test]
    fn an_unknown_argument_never_starts_the_server() {
        assert_eq!(
            parse_strs(&["--verison"]),
            Command::Invalid("--verison".into())
        );
        assert_eq!(parse_strs(&["serve"]), Command::Invalid("serve".into()));
        assert_eq!(
            parse_strs(&["--version", "extra"]),
            Command::Invalid("extra".into())
        );
    }

    #[test]
    fn invalid_exits_non_zero_and_version_exits_zero() {
        assert_eq!(run(&Command::Invalid("x".into())), 2);
        assert_eq!(run(&Command::Version), 0);
        assert!(version().starts_with("app-lb "));
    }
}
