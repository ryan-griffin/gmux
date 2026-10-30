//! Explicit local daemon acquisition and completed shutdown, separate from connecting.
use crate::{ipc::Endpoint, runtime};
use gmux_platform::unix::{ipc::Cancellation, storage};
use gmux_protocol::{control::DaemonAlreadyRunning, control::InternalRequest, transport};
use std::{
    os::fd::{FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::process::Command;

/// Start this gmux executable in a detached session. Reject an existing
/// daemon and return only after the new process's control handshake succeeds.
pub fn start_daemon() -> anyhow::Result<()> {
    runtime::block_on(async {
        let launcher = runtime::blocking(|| std::env::current_exe().map(Launcher::new)).await?;
        launcher.launch(ExistingDaemon::Reject, None).await
    })
}

/// Attach to local execution, starting the packaged CLI alongside this client if needed.
pub async fn ensure_daemon(cancellation: Option<&Cancellation>) -> anyhow::Result<()> {
    let launcher = runtime::cancellable(
        cancellation,
        runtime::blocking(|| {
            std::env::current_exe()
                .map(|p| Launcher::new(p.with_file_name("gmux")))
                .map_err(Into::into)
        }),
    )
    .await?;
    launcher.launch(ExistingDaemon::Connect, cancellation).await
}

#[derive(PartialEq, Eq)]
enum ExistingDaemon {
    Reject,
    Connect,
}

struct Launcher {
    command: Command,
    endpoint: Endpoint,
    log_directory: PathBuf,
    timeout: Duration,
}
impl Launcher {
    fn new(executable: PathBuf) -> Self {
        let mut command = Command::new(executable);
        command.arg("serve");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            command,
            endpoint: Endpoint::local(),
            log_directory: storage::config_dir(),
            timeout: Duration::from_secs(8),
        }
    }

    async fn launch(
        self,
        existing: ExistingDaemon,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<()> {
        let Self {
            mut command,
            endpoint,
            log_directory,
            timeout,
        } = self;
        let deadline = Instant::now() + timeout;
        let mut child = StartedChild(None);
        let result = async {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            if existing == ExistingDaemon::Reject {
                (command, child) =
                    spawn_daemon(command, log_directory.clone(), cancellation.cloned()).await?;
            }
            loop {
                if let Some(cancellation) = cancellation {
                    cancellation.check()?;
                }
                // Cap the handshake as well as the retry delay. A listening process
                // that never replies must not outlive the startup deadline.
                let hello = tokio::time::timeout_at(
                    deadline.into(),
                    endpoint
                        .call_async(serde_json::to_value(InternalRequest::Hello)?, cancellation),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Timed out starting daemon"))?;
                if let Ok(value) = hello {
                    if existing == ExistingDaemon::Reject
                        && value["pid"].as_u64()
                            != child.0.as_ref().and_then(|p| p.id()).map(u64::from)
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
                        return Ok(());
                    }
                } else if child.0.is_none() {
                    (command, child) =
                        spawn_daemon(command, log_directory.clone(), cancellation.cloned()).await?;
                }
                if let Some(process) = child.0.as_mut()
                    && let Some(status) = process.try_wait()?
                {
                    child.0.take();
                    if status.code() != Some(i32::from(DaemonAlreadyRunning::EXIT_CODE)) {
                        anyhow::bail!(
                            "Daemon exited with {status}. See daemon.log in {}",
                            log_directory.display()
                        );
                    }
                    if existing == ExistingDaemon::Reject {
                        return Err(DaemonAlreadyRunning.into());
                    }
                    // Another launcher won the execution lock. Wait for its handshake.
                }
                anyhow::ensure!(Instant::now() < deadline, "Timed out starting daemon");
                runtime::cancellable(cancellation, async {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    Ok(())
                })
                .await?;
            }
        }
        .await;
        if result.is_err()
            && !cancellation.is_some_and(|c| c.check().is_err())
            && let Some(process) = child.0.as_mut()
        {
            let _ = process.start_kill();
            let _ = process.wait().await;
            child.0.take();
        }
        // Success and cancellation both leave shared execution alive. The guard
        // also covers a dropped future, including cancellation during spawn.
        result
    }
}

async fn spawn_daemon(
    mut command: Command,
    log_directory: PathBuf,
    cancellation: Option<Cancellation>,
) -> anyhow::Result<(Command, StartedChild)> {
    // Cancel queued work promptly. If fork/exec has already started, its child
    // guard still detaches and reaps execution when the result is dropped.
    let queued_cancellation = cancellation.clone();
    runtime::cancellable(
        queued_cancellation.as_ref(),
        runtime::blocking(move || {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(cancellation) = &cancellation {
                cancellation.check()?;
            }
            let exe = Path::new(command.as_std().get_program());
            anyhow::ensure!(
                exe.is_file(),
                "Missing {}. Install the gmux package alongside the GTK client.",
                exe.display()
            );
            std::fs::create_dir_all(&log_directory)?;
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(log_directory.join("daemon.log"))?;
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log);
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            let child = StartedChild(Some(command.spawn()?));
            Ok((command, child))
        }),
    )
    .await
}

/// Reap launched processes on the shared executor without a waiting thread per child.
struct StartedChild(Option<tokio::process::Child>);
impl Drop for StartedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            runtime::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

pub(crate) async fn stop(
    endpoint: &Endpoint,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<serde_json::Value> {
    let hello = endpoint
        .call_async(serde_json::to_value(InternalRequest::Hello)?, cancellation)
        .await?;
    stop_after_hello(endpoint, hello, cancellation).await
}

/// An absent local daemon is already stopped. Never retry after sending Hello.
pub(crate) async fn stop_if_running(
    control: &Path,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<serde_json::Value> {
    let Some(hello) = Endpoint::call_unix_if_running(
        control,
        serde_json::to_value(InternalRequest::Hello)?,
        cancellation,
    )
    .await?
    else {
        return Ok(serde_json::json!({"ok":true}));
    };
    let endpoint = Endpoint::unix(
        control.to_owned(),
        gmux_platform::unix::ipc::companion_at(control, ".stream"),
    );
    stop_after_hello(&endpoint, hello, cancellation).await
}

async fn stop_after_hello(
    endpoint: &Endpoint,
    hello: serde_json::Value,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<serde_json::Value> {
    use gmux_protocol::control::ExecutionRequest;
    let pid = hello["pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| anyhow::anyhow!("Daemon handshake requires a process ID"))?;
    // Open before sending Stop to track this process through shutdown, including
    // release of the execution lock, without looking up its PID again.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    anyhow::ensure!(
        fd >= 0,
        "Could not watch daemon shutdown: {}",
        std::io::Error::last_os_error()
    );
    // OwnedFd keeps the descriptor stable until its registration is dropped.
    let process = unsafe { tokio::io::unix::AsyncFd::register(OwnedFd::from_raw_fd(fd)) }?;
    let reply = endpoint
        .execute_async(&ExecutionRequest::Stop, cancellation)
        .await?;
    runtime::cancellable(cancellation, async {
        let _ready =
            tokio::time::timeout(gmux_protocol::network::CONTROL_TIMEOUT, process.readable())
                .await
                .map_err(|_| anyhow::anyhow!("Timed out stopping daemon"))??;
        Ok(reply)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gmux-launch-{}",
                gmux_platform::unix::identity::daemon_id().unwrap()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn launcher(&self) -> Launcher {
            let mut launcher = Launcher::new(PathBuf::from("/bin/sh"));
            launcher.command = Command::new("/bin/sh");
            launcher
                .command
                .args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "gmux-test"]);
            launcher.command.arg(self.0.join("pid"));
            launcher.endpoint = Endpoint::unix(self.0.join("control"), self.0.join("stream"));
            launcher.log_directory = self.0.join("logs");
            launcher.timeout = Duration::from_secs(2);
            launcher
        }
        async fn pid(&self) -> i32 {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(pid) = std::fs::read_to_string(self.0.join("pid"))
                        && let Ok(pid) = pid.trim().parse()
                    {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Also clean up on assertion failure. Successful tests verify reaping
            // before this fallback runs, so zombies cannot pass unnoticed.
            if let Ok(pid) = std::fs::read_to_string(self.0.join("pid"))
                && let Ok(pid) = pid.trim().parse::<i32>()
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn reaped(pid: i32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if unsafe { libc::kill(pid, 0) } < 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH)
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Launched child was not reaped");
    }

    #[test]
    fn acquisition_cancels_while_all_blocking_slots_are_occupied() {
        use std::sync::{Arc, Condvar, Mutex};
        struct OccupiedPool(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for OccupiedPool {
            fn drop(&mut self) {
                let (lock, released) = &*self.0;
                *lock.lock().unwrap() = true;
                released.notify_all();
            }
        }
        let fixture = Fixture::new();
        runtime::block_on(async {
            let pool = OccupiedPool(Arc::new((Mutex::new(false), Condvar::new())));
            let started = Arc::new(tokio::sync::Semaphore::new(0));
            let mut workers = Vec::new();
            for _ in 0..4 {
                let gate = pool.0.clone();
                let started = started.clone();
                workers.push(runtime::spawn(runtime::blocking(move || {
                    let (lock, released) = &*gate;
                    started.add_permits(1);
                    let _guard = released
                        .wait_while(lock.lock().unwrap(), |open| !*open)
                        .unwrap();
                })));
            }
            tokio::time::timeout(Duration::from_secs(2), started.acquire_many(4))
                .await
                .unwrap()
                .unwrap()
                .forget();
            let cancellation = Cancellation::default();
            let mut discovery = Box::pin(ensure_daemon(Some(&cancellation)));
            let mut spawning = Box::pin(
                fixture
                    .launcher()
                    .launch(ExistingDaemon::Reject, Some(&cancellation)),
            );
            // Poll both operations into their queued pool acquisitions before canceling.
            tokio::select! {
                biased;
                result = &mut discovery => panic!("Discovery bypassed the occupied pool: {result:?}"),
                result = &mut spawning => panic!("Spawn bypassed the occupied pool: {result:?}"),
                _ = tokio::task::yield_now() => {},
            }
            cancellation.cancel();
            tokio::time::timeout(Duration::from_millis(300), async {
                assert_eq!(discovery.await.unwrap_err().to_string(), "Request canceled");
                assert_eq!(spawning.await.unwrap_err().to_string(), "Request canceled");
            })
            .await
            .expect("Cancellation waited for a blocking slot");
            drop(pool);
            for worker in workers {
                worker.await.unwrap();
            }
        });
        assert!(!fixture.0.join("logs").exists());
        assert!(!fixture.0.join("pid").exists());
    }

    #[test]
    fn optional_stop_uses_one_hello_and_waits_for_process_exit() {
        let fixture = Fixture::new();
        runtime::block_on(async {
            let control = fixture.0.join("control");
            let listener = tokio::net::UnixListener::bind(&control).unwrap();
            let mut process = Command::new("/bin/sleep")
                .arg("30")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = process.id().unwrap();
            let (replied, reply_received) = tokio::sync::oneshot::channel();
            let (exit, exit_requested) = tokio::sync::oneshot::channel();
            let server = runtime::spawn(async move {
                let (mut hello, _) = listener.accept().await.unwrap();
                let request =
                    crate::io::read_json(&mut tokio::io::BufReader::new(&mut hello), 4096)
                        .await
                        .unwrap();
                assert_eq!(
                    request,
                    serde_json::to_value(InternalRequest::Hello).unwrap()
                );
                crate::io::write_json(&mut hello, &serde_json::json!({"pid":pid}))
                    .await
                    .unwrap();
                let (mut stop, _) = listener.accept().await.unwrap();
                let request = crate::io::read_json(&mut tokio::io::BufReader::new(&mut stop), 4096)
                    .await
                    .unwrap();
                assert_eq!(
                    request,
                    serde_json::json!({"method":"_execution","request":{"method":"stop"}})
                );
                crate::io::write_json(&mut stop, &serde_json::json!({"ok":true}))
                    .await
                    .unwrap();
                replied.send(()).unwrap();
                exit_requested.await.unwrap();
                process.kill().await.unwrap();
            });
            let mut stopping = runtime::spawn(async move { stop_if_running(&control, None).await });
            tokio::time::timeout(Duration::from_secs(2), reply_received)
                .await
                .unwrap()
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut stopping)
                    .await
                    .is_err(),
                "Stop returned while execution was still running"
            );
            exit.send(()).unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(2), stopping)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(reply, serde_json::json!({"ok":true}));
            server.await.unwrap();
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        });
    }

    #[test]
    fn optional_stop_only_succeeds_offline_before_connecting() {
        let fixture = Fixture::new();
        runtime::block_on(async {
            let control = fixture.0.join("control");
            assert_eq!(
                stop_if_running(&control, None).await.unwrap(),
                serde_json::json!({"ok":true})
            );
            // A stale socket is also offline, but a failed or canceled handshake
            // after connecting must not be reported as a successful stop.
            let listener = UnixListener::bind(&control).unwrap();
            use std::os::fd::AsRawFd;
            assert_eq!(
                unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) },
                0
            );
            drop(listener);
            assert!(
                stop_if_running(&control, None).await.unwrap()["ok"]
                    .as_bool()
                    .unwrap()
            );
            std::fs::remove_file(&control).unwrap();
            for cancel in [false, true] {
                let listener = tokio::net::UnixListener::bind(&control).unwrap();
                let cancellation = Cancellation::default();
                let pending = cancellation.clone();
                let server = runtime::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    crate::io::read_json(&mut tokio::io::BufReader::new(&mut stream), 4096)
                        .await
                        .unwrap();
                    if cancel {
                        pending.cancel();
                    }
                    // Close the connected socket without a reply.
                });
                let error = tokio::time::timeout(
                    Duration::from_secs(2),
                    stop_if_running(&control, Some(&cancellation)),
                )
                .await
                .unwrap()
                .unwrap_err();
                if cancel {
                    assert_eq!(error.to_string(), "Request canceled");
                }
                server.await.unwrap();
                std::fs::remove_file(&control).unwrap();
            }
        });
    }

    #[test]
    fn readiness_timeout_bounds_a_silent_handshake_and_reaps_the_launched_child() {
        let fixture = Fixture::new();
        // Listen without replying. A transport response timeout is much longer
        // than this launch deadline, so a retry-only deadline would fail here.
        let _listener = UnixListener::bind(fixture.0.join("control")).unwrap();
        let mut launcher = fixture.launcher();
        launcher.timeout = Duration::from_millis(300);
        runtime::block_on(async {
            let started = Instant::now();
            let error = launcher
                .launch(ExistingDaemon::Reject, None)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "Timed out starting daemon");
            assert!(started.elapsed() < Duration::from_secs(2));
            reaped(fixture.pid().await).await;
        });
        // Clients never unlink the daemon's sockets, even after a failed launch.
        assert!(fixture.0.join("control").exists());
    }

    #[test]
    fn startup_failure_reports_exit_status_and_log_location() {
        let fixture = Fixture::new();
        let mut launcher = fixture.launcher();
        launcher.command = Command::new("/bin/sh");
        launcher
            .command
            .args(["-c", "echo startup-failed >&2; exit 17"]);
        let error = runtime::block_on(launcher.launch(ExistingDaemon::Reject, None)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Daemon exited with exit status: 17"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains(fixture.0.join("logs").to_str().unwrap()),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("logs/daemon.log")).unwrap(),
            "startup-failed\n"
        );
    }

    #[test]
    fn canceled_acquisition_leaves_execution_alive_and_reaps_it_after_exit() {
        let fixture = Fixture::new();
        let _listener = UnixListener::bind(fixture.0.join("control")).unwrap();
        let launcher = fixture.launcher();
        let cancellation = Cancellation::default();
        let requests = cancellation.clone();
        let task = runtime::spawn(async move {
            launcher
                .launch(ExistingDaemon::Reject, Some(&requests))
                .await
        });
        runtime::block_on(async {
            let pid = fixture.pid().await;
            cancellation.cancel();
            assert_eq!(
                task.await.unwrap().unwrap_err().to_string(),
                "Request canceled"
            );
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "The daemon must survive its client"
            );
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            reaped(pid).await;
        });
    }

    #[test]
    fn dropping_acquisition_detaches_and_reaps_its_child() {
        let fixture = Fixture::new();
        let _listener = UnixListener::bind(fixture.0.join("control")).unwrap();
        let launcher = fixture.launcher();
        let task = runtime::spawn(launcher.launch(ExistingDaemon::Reject, None));
        runtime::block_on(async {
            let pid = fixture.pid().await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            reaped(pid).await;
        });
    }

    #[test]
    fn cancellation_before_launch_does_not_create_logs_or_a_process() {
        let fixture = Fixture::new();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let error = runtime::block_on(
            fixture
                .launcher()
                .launch(ExistingDaemon::Reject, Some(&cancellation)),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Request canceled");
        assert!(!fixture.0.join("logs").exists());
        assert!(!fixture.0.join("pid").exists());
    }
}
