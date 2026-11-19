mod cache;
mod proxy;
mod server;
mod session;

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing::{Level, error};

/// Keeps a stdio MCP server asleep until a tool is called.
///
/// Answers the handshake and the tool, prompt and resource lists from a cache,
/// starts SERVER on the first other request, and stops it after the idle time.
#[derive(Debug, Parser)]
#[command(
    version,
    subcommand_negates_reqs = true,
    arg_required_else_help = true,
    disable_help_subcommand = true,
    override_usage = "mcp-nap [OPTIONS] <SERVER>...\n       mcp-nap cache <list|clear> [OPTIONS] [SERVER]..."
)]
struct Cli {
    /// Seconds without activity before the server is stopped, 0 keeps it running
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    idle: u64,

    #[command(flatten)]
    shared: Shared,

    /// The server command and its arguments, passed through untouched
    #[arg(
        value_name = "SERVER",
        required = true,
        trailing_var_arg = true,
        long_help = "The server command and its arguments, passed through untouched.\n\n\
                     Options end at the first argument without a dash, so the server's own flags \
                     are left alone. Write -- before a server that starts with a dash or is named cache."
    )]
    server: Vec<String>,

    #[command(subcommand)]
    action: Option<Action>,
}

/// Options of both running a server and the cache commands.
#[derive(Debug, Args)]
struct Shared {
    /// Where cached results are kept [default: the platform cache folder]
    #[arg(
        long,
        value_name = "DIR",
        global = true,
        long_help = "Where cached results are kept.\n\n\
                     [default: ~/Library/Caches/mcp-nap on macOS, $XDG_CACHE_HOME/mcp-nap or \
                     ~/.cache/mcp-nap on Linux, %LOCALAPPDATA%\\mcp-nap on Windows]"
    )]
    cache_dir: Option<PathBuf>,

    /// Keep a separate cache per working folder
    #[arg(
        long,
        global = true,
        long_help = "Keep a separate cache per working folder, for servers whose tools, prompts \
                     or resources depend on the project they run in."
    )]
    per_dir: bool,

    /// Log errors only
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Also log each request answered from the cache or forwarded to the server
    #[arg(long, short, global = true)]
    verbose: bool,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Show or clear cached results
    #[command(subcommand, disable_help_subcommand = true)]
    Cache(CacheAction),
}

#[derive(Debug, Subcommand)]
enum CacheAction {
    /// Show what is cached for SERVER, or for every server
    List {
        #[arg(value_name = "SERVER", trailing_var_arg = true)]
        server: Vec<String>,
    },
    /// Remove the cache of SERVER, or of every server
    Clear {
        #[arg(value_name = "SERVER", trailing_var_arg = true)]
        server: Vec<String>,
    },
}

/// Parses the command line, rejecting `--idle` with a cache command, where it means nothing.
fn parse_cli<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = Cli::command().try_get_matches_from(args)?;
    let cli = Cli::from_arg_matches(&matches)?;
    if cli.action.is_some() && matches.value_source("idle") == Some(ValueSource::CommandLine) {
        return Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "--idle only applies when running a server",
        ));
    }
    Ok(cli)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = parse_cli(env::args_os()).unwrap_or_else(|error| error.exit());
    let level = match (cli.shared.quiet, cli.shared.verbose) {
        (true, _) => Level::ERROR,
        (_, true) => Level::DEBUG,
        _ => Level::INFO,
    };
    // stdout carries the MCP messages, so logs go to stderr
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_max_level(level)
        .init();

    let cache_dir = cli.shared.cache_dir.unwrap_or_else(cache::default_dir);
    let per_dir = cli.shared.per_dir;
    let result = match cli.action {
        Some(Action::Cache(CacheAction::List { server })) => {
            list_cache(&cache_dir, &server, per_dir)
        }
        Some(Action::Cache(CacheAction::Clear { server })) => {
            clear_cache(&cache_dir, &server, per_dir)
        }
        None => {
            let config = proxy::Config {
                command: cli.server,
                idle: Duration::from_secs(cli.idle),
                cache_dir,
                per_dir,
            };
            proxy::run(config).await
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            error!("{failure}");
            ExitCode::FAILURE
        }
    }
}

fn list_cache(dir: &Path, server: &[String], per_dir: bool) -> io::Result<()> {
    let working_dir = working_dir(server, per_dir)?;
    let only = (!server.is_empty()).then_some((server, working_dir.as_deref()));
    let summaries = cache::summaries(dir, only, SystemTime::now())?;
    for summary in &summaries {
        let program = summary
            .program
            .as_deref()
            .unwrap_or("a file from an older mcp-nap");
        match &summary.server {
            Some(server) => println!("{server} ({program})"),
            None => println!("{program}"),
        }
        if let Some(folder) = &summary.working_dir {
            println!("  for {}", folder.display());
        }
        for entry in &summary.entries {
            let items = entry
                .items
                .map(|count| format!("{count} item{}, ", if count == 1 { "" } else { "s" }))
                .unwrap_or_default();
            let freshness = match entry.freshness {
                cache::Freshness::UntilChanged => {
                    "kept until the server lists something else".to_owned()
                }
                cache::Freshness::Fresh(left) => format!("fresh for {}", human(left)),
                cache::Freshness::Stale => "stale, asked again next time".to_owned(),
            };
            println!("  {:<28} {items}{freshness}", entry.key);
        }
    }
    let plural = if summaries.len() == 1 { "" } else { "s" };
    println!(
        "{} cached server{plural} in {}",
        summaries.len(),
        dir.display()
    );
    Ok(())
}

fn clear_cache(dir: &Path, server: &[String], per_dir: bool) -> io::Result<()> {
    let working_dir = working_dir(server, per_dir)?;
    let only = (!server.is_empty()).then_some((server, working_dir.as_deref()));
    let removed = cache::clear(dir, only)?;
    let plural = if removed == 1 { "" } else { "s" };
    println!(
        "removed {removed} cache file{plural} from {}",
        dir.display()
    );
    Ok(())
}

/// The folder a `--per-dir` cache belongs to, when a server is given.
fn working_dir(server: &[String], per_dir: bool) -> io::Result<Option<PathBuf>> {
    (per_dir && !server.is_empty())
        .then(env::current_dir)
        .transpose()
}

fn human(duration: Duration) -> String {
    match duration.as_secs() {
        seconds if seconds < 60 => format!("{seconds} s"),
        seconds if seconds < 3600 => format!("{} min", seconds / 60),
        seconds => format!("{} h", seconds / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &str) -> Result<Cli, clap::Error> {
        parse_cli(std::iter::once("mcp-nap").chain(args.split_whitespace()))
    }

    fn cache_action(args: &str) -> CacheAction {
        match parse(args).unwrap().action {
            Some(Action::Cache(action)) => action,
            None => panic!("expected a cache command for {args:?}"),
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn options_end_at_the_server_command() {
        let cli = parse("--idle 30 --per-dir -v npx -y server --idle 5").unwrap();
        assert_eq!(cli.idle, 30);
        assert!(cli.shared.per_dir && cli.shared.verbose);
        assert_eq!(cli.server, ["npx", "-y", "server", "--idle", "5"]);
        assert!(cli.action.is_none());
    }

    #[test]
    fn defaults_to_ten_idle_minutes_and_one_cache_per_command() {
        let cli = parse("server").unwrap();
        assert_eq!(cli.idle, 600);
        assert!(!cli.shared.per_dir);
        assert_eq!(cli.shared.cache_dir, None);
    }

    #[test]
    fn cache_list_and_clear_take_an_optional_server() {
        assert!(
            matches!(cache_action("cache list"), CacheAction::List { server } if server.is_empty())
        );
        assert!(matches!(
            cache_action("cache clear npx -y server --flag"),
            CacheAction::Clear { server } if server == ["npx", "-y", "server", "--flag"]
        ));
    }

    #[test]
    fn shared_options_work_before_or_after_the_cache_command() {
        let before = parse("--cache-dir /tmp/nap --per-dir cache list").unwrap();
        let after = parse("cache list --cache-dir /tmp/nap --per-dir").unwrap();
        for cli in [before, after] {
            assert_eq!(cli.shared.cache_dir, Some(PathBuf::from("/tmp/nap")));
            assert!(cli.shared.per_dir);
            assert!(matches!(
                cli.action,
                Some(Action::Cache(CacheAction::List { .. }))
            ));
        }
    }

    #[test]
    fn a_server_named_cache_runs_after_a_double_dash_or_another_word() {
        assert_eq!(
            parse("-- cache --flag").unwrap().server,
            ["cache", "--flag"]
        );
        assert_eq!(parse("server cache").unwrap().server, ["server", "cache"]);
        assert_eq!(parse("-- -server").unwrap().server, ["-server"]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("").is_err());
        assert!(parse("--").is_err());
        assert!(parse("cache").is_err());
        assert!(parse("--idle soon server").is_err());
        assert!(parse("--unknown server").is_err());
        assert!(parse("-q -v server").is_err());
        assert!(parse("--idle 5 cache list").is_err());
    }
}
