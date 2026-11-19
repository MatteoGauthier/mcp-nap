use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::proxy::Event;

/// How long a stopped server gets to exit before its process group is killed.
const GRACE: Duration = Duration::from_secs(2);

/// The real MCP server, running in its own process group.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
}

impl Server {
    pub fn spawn(command: &[String], generation: u64, events: Sender<Event>) -> io::Result<Server> {
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if events.send(Event::Server(generation, line)).is_err() {
                    return;
                }
            }
            let _ = events.send(Event::ServerClosed(generation));
        });
        Ok(Server { child, stdin })
    }

    pub fn send(&mut self, line: &str) -> io::Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    /// Stops the whole process group, so launchers like `npx` take their children with them.
    pub fn stop(self) -> JoinHandle<()> {
        let Server { mut child, stdin } = self;
        drop(stdin);
        let group = -(child.id() as libc::pid_t);
        unsafe { libc::kill(group, libc::SIGTERM) };
        thread::spawn(move || {
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unsafe { libc::kill(group, libc::SIGKILL) };
            let _ = child.wait();
        })
    }
}
