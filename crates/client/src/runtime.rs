//! Shared client I/O executor. Blocking application work uses a bounded pool.
use gmux_platform::unix::ipc::Cancellation;
use std::{
    future::Future,
    sync::{Arc, OnceLock},
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
    task::JoinHandle,
};

fn executor() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            // Four application workers leave room for the system resolver.
            .max_blocking_threads(8)
            .thread_name("gmux-client")
            .enable_all()
            .build()
            .expect("Could not start client I/O executor")
    })
}

pub(crate) fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    executor().spawn(future)
}

/// Synchronous entry for CLI callers and shutdown. Never call on the I/O thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    executor().block_on(future)
}

/// Run blocking application work without allocating a thread per queue.
pub async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    blocking_with_slots(
        SLOTS.get_or_init(|| Arc::new(Semaphore::new(4))).clone(),
        work,
    )
    .await
}

async fn blocking_with_slots<R: Send + 'static>(
    slots: Arc<Semaphore>,
    work: impl FnOnce() -> R + Send + 'static,
) -> R {
    let slot = slots.acquire_owned().await.expect("Blocking pool closed");
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .expect("Client blocking worker panicked")
}

pub(crate) async fn cancellable<T>(
    cancellation: Option<&Cancellation>,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(cancellation) = cancellation else {
        return future.await;
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("Request canceled"),
        result = future => result,
    }
}

pub(crate) async fn deadline<T>(
    deadline: std::time::Instant,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout_at(deadline.into(), future)
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "Connection deadline expired")
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn bounded_workers_leave_the_io_executor_responsive() {
        let slots = Arc::new(Semaphore::new(2));
        let (started, ready) = mpsc::channel();
        let mut releases = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..2 {
            let (release, wait) = mpsc::channel();
            releases.push(release);
            let started = started.clone();
            let slots = slots.clone();
            workers.push(spawn(async move {
                blocking_with_slots(slots, move || {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                })
                .await
            }));
        }
        for _ in 0..2 {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let (extra, entered) = mpsc::channel();
        let worker =
            spawn(async move { blocking_with_slots(slots, move || extra.send(()).unwrap()).await });
        // This task represents timer, heartbeat and input-writer work. It must
        // run while every worker is busy and another decode is waiting.
        let (progress, progressed) = mpsc::channel();
        spawn(async move {
            progress.send(()).unwrap();
        });
        progressed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(entered.try_recv().is_err());
        for release in releases {
            release.send(()).unwrap();
        }
        for worker in workers {
            block_on(worker).unwrap();
        }
        block_on(worker).unwrap();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
