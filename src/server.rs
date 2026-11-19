use std::io;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop, ProcessGroup};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::signal::unix::SignalKind;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;

/// A line or the end of output from one server, tagged with the generation it was started as.
pub enum Output {
    Line(u64, String),
    Closed(u64),
}

/// The real MCP server, leader of its own process group.
pub struct Server {
    child: Box<dyn ChildWrapper>,
    stdin: Option<ChildStdin>,
}

impl Server {
    pub fn spawn(
        command: &[String],
        generation: u64,
        output: UnboundedSender<Output>,
    ) -> io::Result<Server> {
        let mut wrap = CommandWrap::with_new(&command[0], |child| {
            child
                .args(&command[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
        });
        wrap.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
        let mut child = wrap.spawn()?;
        let stdin = child.stdin().take();
        let stdout = child.stdout().take().expect("stdout is piped");
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if output.send(Output::Line(generation, line)).is_err() {
                    return;
                }
            }
            let _ = output.send(Output::Closed(generation));
        });
        Ok(Server { child, stdin })
    }

    pub async fn send(&mut self, line: &str) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    /// The stdio shutdown of the MCP spec: close stdin, then SIGTERM, then SIGKILL, waiting `grace` between each.
    pub async fn stop(mut self, grace: Duration) {
        drop(self.stdin.take());
        if self.exited_within(grace).await {
            return;
        }
        let _ = self.child.signal(SignalKind::terminate().as_raw_value());
        if self.exited_within(grace).await {
            return;
        }
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    async fn exited_within(&mut self, grace: Duration) -> bool {
        timeout(grace, self.child.wait()).await.is_ok()
    }
}
