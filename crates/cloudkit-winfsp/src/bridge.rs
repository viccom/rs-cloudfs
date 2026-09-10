//! The async bridge: WinFsp dispatcher threads -> tokio.
//!
//! The FSD calls every [`winfsp::filesystem::FileSystemContext`] method
//! on its own thread pool, synchronously; the semantic core
//! (`cloudkit-core`) is async. This type carries the process runtime
//! handle into the adapter and blocks a dispatcher thread on a future
//! when it needs one.
//!
//! Shape (decided in WF1, spike-verified): a *clone of the runtime
//! handle*, not a runtime the adapter builds — the mount assembly (WF4)
//! owns the process runtime, so `Handle::block_on` from a non-runtime
//! thread is legal and deadlock-free for the callback pattern (winfsp
//! invokes callbacks on plain OS threads; no callback re-enters the
//! runtime from inside a poll). rclone's `cmount` uses the same blocking
//! shape; the escaping valve if the thread pool ever stalls under real
//! load is the WF2+ bounded wrapper, not a queue here.
//!
//! WF1 uses the bridge only for cheap local work; the read/write paths
//! (WF2/WF3) are the ones actually awaiting network futures on it.

use std::future::Future;

/// A `tokio::runtime::Handle` the adapter blocks on.
#[derive(Clone, Debug)]
pub struct AsyncBridge {
    rt: tokio::runtime::Handle,
}

impl AsyncBridge {
    /// Wraps the process runtime handle (the caller keeps ownership).
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self { rt }
    }

    /// Blocks the calling dispatcher thread until `future` completes.
    ///
    /// Must not be called from inside a task of `self.rt` — the FSD
    /// never does (callbacks arrive on its own threads). No timeout
    /// wrapper: rclone's lesson is that bounding callbacks adds failure
    /// modes; the cure is fewer callbacks and cached answers (K44/K41).
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.rt.block_on(future)
    }

    /// The injected runtime handle (WF2+ uses it to spawn detached work,
    /// e.g. the handle grace-period timer).
    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.rt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    /// The dispatcher-thread shape: a plain OS thread blocks on a real
    /// future and gets its output back.
    #[test]
    fn block_on_returns_the_output_from_a_plain_thread() {
        let rt = test_runtime();
        let bridge = AsyncBridge::new(rt.handle().clone());
        let out = std::thread::spawn(move || bridge.block_on(async { 40 + 2 }))
            .join()
            .expect("bridge thread");
        assert_eq!(out, 42);
    }

    /// The future must run ON the injected runtime: a driver that only
    /// borrows a handle (timer/IO context, spawn targets) depends on the
    /// identity, not just on "some runtime".
    #[test]
    fn block_on_runs_the_future_on_the_injected_runtime() {
        let rt = test_runtime();
        let bridge = AsyncBridge::new(rt.handle().clone());
        let observed = bridge.block_on(async { tokio::runtime::Handle::current().id() });
        assert_eq!(observed, rt.handle().id());
    }

    /// The injected runtime's own worker threads are what keep timers and
    /// spawned tasks alive while the dispatcher thread is blocked — the
    /// property that makes `Handle::block_on` safe here.
    #[test]
    fn tasks_spawned_on_the_injected_runtime_progress_while_blocked() {
        let rt = test_runtime();
        let bridge = AsyncBridge::new(rt.handle().clone());
        let ticks = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&ticks);
        let _keep = rt.spawn(async move {
            loop {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        bridge.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        assert!(
            ticks.load(Ordering::Relaxed) > 0,
            "the injected runtime made no progress while the bridge was blocked"
        );
    }

    /// Dispatchers are multi-threaded: the bridge is shared, not moved.
    #[test]
    fn bridge_is_shareable_across_dispatcher_threads() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<AsyncBridge>();
    }
}
