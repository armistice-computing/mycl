//! Opt-in monotonic stage timings. Never record query text, content or credentials.
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
thread_local! { static RUN: Cell<Option<u64>> = const { Cell::new(None) }; }
pub(crate) fn new_run() -> Option<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    (std::env::var("GARDEN_SEARCH_TIMING").as_deref() == Ok("1"))
        .then(|| NEXT.fetch_add(1, Ordering::Relaxed))
}
pub(crate) fn with_run<T>(id: Option<u64>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUN.with(|r| r.set(self.0));
        }
    }
    let _restore = Restore(RUN.with(|r| r.replace(id)));
    work()
}
pub(crate) struct Stage {
    id: Option<u64>,
    name: &'static str,
    start: Instant,
}
impl Stage {
    pub(crate) fn for_run(id: Option<u64>, name: &'static str) -> Self {
        Self {
            id,
            name,
            start: Instant::now(),
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            log::info!(target: "search_timing", "search_id={} stage={} elapsed_us={}", id, self.name, self.start.elapsed().as_micros());
        }
    }
}
pub(crate) fn stage(name: &'static str) -> Stage {
    Stage::for_run(RUN.with(Cell::get), name)
}
pub(crate) fn event(name: &'static str) {
    drop(stage(name));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_trace_context_restores_after_nesting_and_panic() {
        assert_eq!(RUN.with(Cell::get), None);
        with_run(Some(7), || {
            with_run(Some(8), || assert_eq!(RUN.with(Cell::get), Some(8)));
            assert_eq!(RUN.with(Cell::get), Some(7));
            let _ = std::panic::catch_unwind(|| with_run(Some(9), || panic!("probe")));
            assert_eq!(RUN.with(Cell::get), Some(7));
        });
        assert_eq!(RUN.with(Cell::get), None);
    }
}

/// Default outcome survives early return, cancellation or unwinding. Workers
/// and callers have separate outcomes because a cancelled caller may leave a
/// blocking worker running until completion.
pub(crate) struct Outcome {
    id: Option<u64>,
    label: &'static str,
}
impl Outcome {
    pub(crate) fn new(id: Option<u64>, label: &'static str) -> Self {
        Self { id, label }
    }
    pub(crate) fn finish(&mut self, label: &'static str) {
        self.label = label;
    }
}
impl Drop for Outcome {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            log::info!(target: "search_timing", "search_id={} outcome={}", id, self.label);
        }
    }
}
