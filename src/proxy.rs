use std::collections::VecDeque;
use std::env;
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{sleep_until, timeout};
use tracing::{error, info, warn};

use crate::cache::Cache;
use crate::server::{Output, Server};
use crate::session::{Action, Session};

pub struct Config {
    pub command: Vec<String>,
    pub idle: Duration,
    pub cache_dir: PathBuf,
    /// keep a separate cache per working folder
    pub per_dir: bool,
}

/// Wait between the stop steps when the server is idle or the client has left.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Wait between the stop steps when the client stops mcp-nap, as clients escalate to a kill within about half a second.
const SIGNAL_GRACE: Duration = Duration::from_millis(150);

pub async fn run(config: Config) -> io::Result<()> {
    let working_dir = config.per_dir.then(env::current_dir).transpose()?;
    let cache = Cache::open(&config.cache_dir, &config.command, working_dir.as_deref());
    let mut session = Session::new(cache, config.idle, Instant::now());

    let (client_lines, mut client) = mpsc::unbounded_channel();
    // a plain thread, so a stdin read that never returns cannot hold up the exit
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() && client_lines.send(line).is_err() {
                return;
            }
        }
    });

    let (server_output, mut output) = mpsc::unbounded_channel();
    let mut stop_signals = StopSignals::new()?;
    let mut io = Io {
        command: config.command,
        server: None,
        generation: 0,
        server_output,
        stopping: JoinSet::new(),
        stdout: tokio::io::stdout(),
    };

    let grace = loop {
        let deadline = session.idle_deadline();
        let actions = tokio::select! {
            line = client.recv() => match line {
                Some(line) => session.on_client(&line, Instant::now()),
                None => break STOP_GRACE,
            },
            Some(event) = output.recv() => match event {
                Output::Line(generation, line) if generation == io.generation => {
                    session.on_server(&line, Instant::now())
                }
                Output::Closed(generation) if generation == io.generation => {
                    warn!("the server exited");
                    session.on_server_exit("mcp-nap: the server exited")
                }
                _ => continue,
            },
            _ = sleep_until(deadline.unwrap_or_else(Instant::now).into()), if deadline.is_some() => {
                session.on_tick(Instant::now())
            }
            () = stop_signals.recv() => break SIGNAL_GRACE,
        };
        if io.perform(actions, &mut session).await.is_err() {
            // the client stopped reading
            break STOP_GRACE;
        }
    };

    if let Some(server) = io.server.take() {
        info!("stopping the server");
        io.stopping.spawn(server.stop(grace));
    }
    let _ = timeout(grace * 3, io.stopping.join_all()).await;
    Ok(())
}

/// The processes and pipes the session's actions act on.
struct Io {
    command: Vec<String>,
    server: Option<Server>,
    /// bumped on every start and stop, so output of an old server is ignored
    generation: u64,
    server_output: mpsc::UnboundedSender<Output>,
    stopping: JoinSet<()>,
    stdout: tokio::io::Stdout,
}

impl Io {
    async fn perform(&mut self, actions: Vec<Action>, session: &mut Session) -> io::Result<()> {
        let mut actions = VecDeque::from(actions);
        while let Some(action) = actions.pop_front() {
            match action {
                Action::Start => {
                    self.generation += 1;
                    match Server::spawn(&self.command, self.generation, self.server_output.clone())
                    {
                        Ok(server) => {
                            info!("started the server");
                            self.server = Some(server);
                        }
                        Err(error) => {
                            error!("cannot start {}: {error}", self.command[0]);
                            let reason =
                                format!("mcp-nap: cannot start {}: {error}", self.command[0]);
                            actions.extend(session.on_server_exit(&reason));
                        }
                    }
                }
                Action::Stop => {
                    self.generation += 1;
                    if let Some(server) = self.server.take() {
                        info!("stopping the server");
                        self.stopping.spawn(server.stop(STOP_GRACE));
                    }
                }
                Action::ToServer(line) => {
                    let Some(server) = self.server.as_mut() else {
                        continue;
                    };
                    if server.send(&line).await.is_err() {
                        actions.extend(session.on_server_exit("mcp-nap: the server exited"));
                    }
                }
                Action::ToClient(line) => {
                    self.stdout.write_all(line.as_bytes()).await?;
                    self.stdout.write_all(b"\n").await?;
                    self.stdout.flush().await?;
                }
            }
        }
        Ok(())
    }
}

/// How a client asks mcp-nap to stop: SIGINT, SIGTERM or SIGHUP on Unix.
#[cfg(unix)]
struct StopSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn new() -> io::Result<StopSignals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(StopSignals {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// How a client asks mcp-nap to stop: Ctrl+C, a closed console or a system shutdown on Windows.
#[cfg(windows)]
struct StopSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl StopSignals {
    fn new() -> io::Result<StopSignals> {
        use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_shutdown};
        Ok(StopSignals {
            ctrl_c: ctrl_c()?,
            close: ctrl_close()?,
            shutdown: ctrl_shutdown()?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.ctrl_c.recv() => {}
            _ = self.close.recv() => {}
            _ = self.shutdown.recv() => {}
        }
    }
}
