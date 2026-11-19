mod cache;
mod proxy;
mod server;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

/// Keeps a stdio MCP server asleep until a tool is called.
///
/// Answers the handshake and the tool, prompt and resource lists from a cache,
/// starts COMMAND on the first other request, and stops it after the idle time.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Seconds without activity before the server is stopped, 0 keeps it running
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    idle: u64,

    /// Where cached results are kept [default: the platform cache folder]
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Keep a separate cache per working folder
    #[arg(long)]
    per_dir: bool,

    /// The server command and its arguments, passed through untouched
    #[arg(value_name = "COMMAND", required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = proxy::Config {
        command: cli.command,
        idle: Duration::from_secs(cli.idle),
        cache_dir: cli.cache_dir.unwrap_or_else(cache::default_dir),
        per_dir: cli.per_dir,
    };
    match proxy::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mcp-nap: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(["mcp-nap"].iter().chain(args))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn options_end_at_the_server_command() {
        let cli = parse(&[
            "--idle",
            "30",
            "--per-dir",
            "npx",
            "-y",
            "server",
            "--idle",
            "5",
        ])
        .unwrap();
        assert_eq!(cli.idle, 30);
        assert!(cli.per_dir);
        assert_eq!(cli.command, ["npx", "-y", "server", "--idle", "5"]);
    }

    #[test]
    fn a_double_dash_allows_a_command_starting_with_a_dash() {
        assert_eq!(parse(&["--", "-server"]).unwrap().command, ["-server"]);
    }

    #[test]
    fn defaults_to_ten_idle_minutes_and_one_cache_per_command() {
        let cli = parse(&["server"]).unwrap();
        assert_eq!(cli.idle, 600);
        assert!(!cli.per_dir);
        assert_eq!(cli.cache_dir, None);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--"]).is_err());
        assert!(parse(&["--idle", "soon", "server"]).is_err());
        assert!(parse(&["--verbose", "server"]).is_err());
    }
}
