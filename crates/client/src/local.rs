//! Explicit lifecycle management for execution on this computer.
use crate::{control::InternalRequest, ipc, transport};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    time::{Duration, Instant},
};

/// Wait for the actual daemon process to exit, including release of its
/// execution lock. A successful stop can be followed immediately by serve.
pub(crate) fn stop(
    endpoint: &ipc::Endpoint,
    cancellation: Option<&ipc::Cancellation>,
) -> anyhow::Result<serde_json::Value> {
    let hello = match cancellation {
        Some(c) => endpoint.call_cancellable(InternalRequest::Hello, c)?,
        None => endpoint.call(InternalRequest::Hello)?,
    };
    let pid = hello["pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| anyhow::anyhow!("Daemon handshake requires a process ID"))?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    anyhow::ensure!(
        fd >= 0,
        "Could not watch daemon shutdown: {}",
        std::io::Error::last_os_error()
    );
    let process = unsafe { OwnedFd::from_raw_fd(fd) };
    let reply = endpoint.execute(&crate::control::ExecutionRequest::Stop, cancellation)?;
    let deadline = Instant::now() + crate::network::CONTROL_TIMEOUT;
    loop {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let mut poll = libc::pollfd {
            fd: process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 25) };
        if result > 0 {
            return Ok(reply);
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        anyhow::ensure!(Instant::now() < deadline, "Timed out stopping daemon");
    }
}

pub fn ensure_daemon(cancellation: Option<&ipc::Cancellation>) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?.with_file_name("gmux");
    let mut command = std::process::Command::new(exe);
    command.arg("serve");
    launch_daemon(command, ExistingDaemon::Connect, cancellation)
}

/// The daemon's execution lock is already held by another process.
#[derive(Debug)]
pub struct DaemonAlreadyRunning;

impl DaemonAlreadyRunning {
    // Distinguish a competing launcher from a startup failure when GTK starts
    // or attaches to a daemon. Both remain errors for an explicit serve command.
    pub const EXIT_CODE: u8 = 3;
}

impl std::fmt::Display for DaemonAlreadyRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "The local daemon is already running or starting. Use gmux connection edit local to change its settings, or gmux stop before starting it again.",
        )
    }
}
impl std::error::Error for DaemonAlreadyRunning {}

#[derive(PartialEq, Eq)]
enum ExistingDaemon {
    Reject,
    Connect,
}

/// Start a foreground daemon command in a detached session. Reject an existing
/// daemon and return only after the new process's control handshake succeeds.
pub fn start_daemon(command: std::process::Command) -> anyhow::Result<()> {
    launch_daemon(command, ExistingDaemon::Reject, None)
}

fn spawn_daemon(command: &mut std::process::Command) -> anyhow::Result<std::process::Child> {
    use std::os::unix::fs::OpenOptionsExt;
    let exe = Path::new(command.get_program());
    anyhow::ensure!(
        exe.is_file(),
        "Missing {}. Install the gmux package alongside the GTK client.",
        exe.display()
    );
    std::fs::create_dir_all(crate::storage::config_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(crate::storage::config_dir().join("daemon.log"))?;
    Ok(command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()?)
}

fn launch_daemon(
    mut command: std::process::Command,
    existing: ExistingDaemon,
    cancellation: Option<&ipc::Cancellation>,
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut child = if existing == ExistingDaemon::Reject {
        Some(spawn_daemon(&mut command)?)
    } else {
        None
    };
    let result = (|| {
        loop {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            let hello = match cancellation {
                Some(c) => ipc::Endpoint::local().call_cancellable(InternalRequest::Hello, c),
                None => ipc::call(InternalRequest::Hello),
            };
            if let Ok(value) = hello {
                if existing == ExistingDaemon::Reject
                    && value["pid"].as_u64() != child.as_ref().map(|child| u64::from(child.id()))
                {
                    return Err(DaemonAlreadyRunning.into());
                }
                anyhow::ensure!(
                    value["version"] == transport::VERSION,
                    "Incompatible gmux daemon. Stop it with gmux stop before upgrading."
                );
                let quitting = value["quitting"].as_bool().ok_or_else(|| {
                    anyhow::anyhow!("Daemon handshake requires a quitting status")
                })?;
                if !quitting {
                    if let Some(mut child) = child.take() {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    return Ok(());
                }
            } else if child.is_none() {
                if let Some(cancellation) = cancellation {
                    cancellation.check()?;
                }
                child = Some(spawn_daemon(&mut command)?);
            }
            if let Some(process) = child.as_mut()
                && let Some(status) = process.try_wait()?
            {
                if status.code() != Some(i32::from(DaemonAlreadyRunning::EXIT_CODE)) {
                    anyhow::bail!(
                        "Daemon exited with {status}. See daemon.log in {}",
                        crate::storage::config_dir().display()
                    );
                }
                if existing == ExistingDaemon::Reject {
                    return Err(DaemonAlreadyRunning.into());
                }
                // GTK may attach to the winner once it finishes starting.
                child = None;
            }
            anyhow::ensure!(Instant::now() < deadline, "Timed out starting daemon");
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    if result.is_err()
        && let Some(process) = child
    {
        abandon_start(process, cancellation);
    }
    result
}

fn abandon_start(mut process: std::process::Child, cancellation: Option<&ipc::Cancellation>) {
    if cancellation.is_some_and(|c| c.check().is_err()) {
        // Canceling a client's attach must not terminate shared execution.
        std::thread::spawn(move || {
            let _ = process.wait();
        });
    } else {
        let _ = process.kill();
        let _ = process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceling_startup_detaches_instead_of_terminating_execution() {
        let process = std::process::Command::new("sleep")
            .arg("10")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = process.id() as i32;
        let cancellation = ipc::Cancellation::default();
        cancellation.cancel();
        abandon_start(process, Some(&cancellation));
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "The daemon must survive its client"
        );
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    }
}
