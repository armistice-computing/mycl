//! Keep CPU/filesystem-heavy searches off the async server workers. A permit
//! lives inside the blocking task, so disconnecting cannot bypass the limit.
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
const OVERLOADED: &str = "search capacity exhausted; retry shortly";

struct Executor {
    workers: Arc<Semaphore>,
    admission: Arc<Semaphore>,
}
impl Executor {
    fn new(workers: usize, queued: usize) -> Self {
        Self {
            workers: Arc::new(Semaphore::new(workers)),
            admission: Arc::new(Semaphore::new(workers + queued)),
        }
    }
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let trace = crate::search_timing::new_run();
        let mut outcome = crate::search_timing::Outcome::new(trace, "caller_cancelled");
        let _request = crate::search_timing::Stage::for_run(trace, "request_total");
        let waiting = crate::search_timing::Stage::for_run(trace, "worker_wait");
        // Admission never waits: only admitted requests may retain queued payloads.
        let admission = self.admission.clone().try_acquire_owned().map_err(|_| {
            outcome.finish("overloaded");
            OVERLOADED.to_string()
        })?;
        let lease = crate::cell_graph_boundary::current_cell_lease();
        let worker = self.workers.clone().acquire_owned().await.map_err(|e| {
            outcome.finish("admission_closed");
            e.to_string()
        })?;
        drop(waiting);
        let pool_wait = crate::search_timing::Stage::for_run(trace, "blocking_pool_wait");
        let result = crate::app_runtime::async_runtime::spawn_blocking(move || {
            let (_admission, _worker) = (admission, worker);
            drop(pool_wait);
            crate::search_timing::with_run(trace, || {
                let _execution = crate::search_timing::stage("execution");
                let mut worker = crate::search_timing::Outcome::new(trace, "worker_panicked");
                let result = crate::cell_graph_boundary::with_cell_lease(lease, work);
                worker.finish(if result.is_ok() {
                    "worker_succeeded"
                } else {
                    "worker_failed"
                });
                result
            })
        })
        .await
        .map_err(|e| {
            outcome.finish("worker_join_failed");
            format!("search worker failed: {e}")
        })?;
        outcome.finish(if result.is_ok() {
            "caller_succeeded"
        } else {
            "caller_failed"
        });
        result
    }
}
pub(crate) async fn run<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    static EXECUTOR: OnceLock<Executor> = OnceLock::new();
    EXECUTOR.get_or_init(|| Executor::new(2, 8)).run(work).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn blocking_search_does_not_block_runtime() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run(move || {
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(7)
        }));
        rx.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!task.is_finished());
        assert_eq!(task.await.unwrap().unwrap(), 7);
        assert!(run::<()>(|| Err("failure".into())).await.is_err());
    }
    #[tokio::test]
    async fn search_concurrency_is_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let active = active.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(run(move || {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn overload_and_cancellation_preserve_capacity() {
        let executor = Arc::new(Executor::new(1, 1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let e = executor.clone();
        let running = tokio::spawn(async move {
            e.run(move || {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        let e = executor.clone();
        let queued = tokio::spawn(async move { e.run(|| Ok(())).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while executor.admission.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(executor.run(|| Ok(())).await.unwrap_err(), OVERLOADED);
        queued.abort();
        let _ = queued.await;
        assert_eq!(executor.admission.available_permits(), 1);
        running.abort();
        let _ = running.await;
        // Dropping the caller must not free a still-running blocking worker.
        assert_eq!(executor.workers.available_permits(), 0);
        assert_eq!(executor.admission.available_permits(), 1);
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), executor.run(|| Ok(())))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(executor.admission.available_permits(), 2);
        assert_eq!(executor.workers.available_permits(), 1);
    }
}
