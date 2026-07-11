//! SSH tunnels for profiles with `ssh = "user@bastion"`.
//!
//! Shells out to OpenSSH (inherits ~/.ssh/config, ProxyJump, agent, MFA)
//! rather than embedding an SSH library. Deliberately avoids `-f`: a
//! self-backgrounding ssh can't be killed reliably, so we keep the child in
//! the foreground of its own process and kill it on drop.

use anyhow::{bail, Context, Result};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Tunnel {
    child: Child,
    pub local_port: u16,
}

impl Tunnel {
    /// Forward 127.0.0.1:<local_port> → host:port via `ssh_dest`.
    pub fn open(ssh_dest: &str, host: &str, port: u16) -> Result<Self> {
        let local_port = free_port()?;
        let child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-N")
            .arg("-L")
            .arg(format!("{local_port}:{host}:{port}"))
            .arg(ssh_dest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning ssh (is OpenSSH installed?)")?;

        let mut tunnel = Tunnel { child, local_port };
        tunnel.wait_ready(Duration::from_secs(12))?;
        Ok(tunnel)
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let addr = format!("127.0.0.1:{}", self.local_port);
        loop {
            if let Some(status) = self.child.try_wait().ok().flatten() {
                bail!(
                    "ssh tunnel exited early ({status}) — check the ssh destination \
                     (BatchMode is on, so interactive prompts fail; use ssh-agent or keys)"
                );
            }
            if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(250))
                .is_ok()
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("ssh tunnel did not come up within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").context("finding a free local port")?;
    Ok(l.local_addr()?.port())
}
