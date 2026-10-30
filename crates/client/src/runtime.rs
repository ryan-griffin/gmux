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
    let slot = SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await
        .expect("Blocking pool closed");
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
