mod cache;
mod proxy;
mod server;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use proxy::Config;

const USAGE: &str = "\
usage: mcp-nap [--idle <seconds>] [--cache-dir <dir>] <command> [args...]

Starts <command> on the first tool call and stops it after <seconds> idle.

options:
  --idle <seconds>   idle time before the server is stopped, 0 = never (default 600)
  --cache-dir <dir>  where handshake and list responses are cached
  -h, --help         print this help
  -V, --version      print the version";

enum Command {
    Run(Config),
    Help,
    Version,
}

fn main() -> ExitCode {
    match parse_args(env::args().skip(1).collect()) {
        Ok(Command::Run(config)) => {
            proxy::run(config);
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("mcp-nap {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("mcp-nap: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Command, String> {
    let mut idle = Duration::from_secs(600);
    let mut cache_dir = None;
    let mut args = args.into_iter().peekable();
    // like `timeout` or `env`: options end at the first argument without a dash, or at `--`
    while let Some(arg) = args.next_if(|arg| arg.starts_with('-')) {
        match arg.as_str() {
            "--" => break,
            "--idle" => {
                let value = args.next().ok_or("--idle needs a value")?;
                let seconds = value
                    .parse()
                    .map_err(|_| format!("invalid --idle value: {value}"))?;
                idle = Duration::from_secs(seconds);
            }
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(
                    args.next().ok_or("--cache-dir needs a value")?,
                ));
            }
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    let command: Vec<String> = args.collect();
    if command.is_empty() {
        return Err("missing server command".into());
    }
    Ok(Command::Run(Config {
        command,
        idle,
        cache_dir: cache_dir.unwrap_or_else(cache::default_dir),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|arg| arg.to_string()).collect())
    }

    fn run(args: &[&str]) -> Config {
        let Ok(Command::Run(config)) = parse(args) else {
            panic!("expected a run command for {args:?}");
        };
        config
    }

    #[test]
    fn options_end_at_the_server_command() {
        let config = run(&[
            "--idle",
            "30",
            "--cache-dir",
            "/tmp/nap",
            "npx",
            "-y",
            "server",
            "--idle",
            "5",
        ]);
        assert_eq!(config.idle, Duration::from_secs(30));
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/nap"));
        assert_eq!(config.command, ["npx", "-y", "server", "--idle", "5"]);
    }

    #[test]
    fn accepts_a_double_dash_before_the_command() {
        assert_eq!(run(&["--idle", "30", "--", "-server"]).command, ["-server"]);
    }

    #[test]
    fn defaults_to_ten_idle_minutes() {
        assert_eq!(run(&["server"]).idle, Duration::from_secs(600));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--"]).is_err());
        assert!(parse(&["--idle", "soon", "server"]).is_err());
        assert!(parse(&["--verbose", "server"]).is_err());
    }
}
