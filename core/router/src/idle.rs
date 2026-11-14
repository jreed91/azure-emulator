//! Noticing when nobody has used the emulator for a while (`--idle-timeout`).
//!
//! Activity is whatever holds a [`Busy`] guard: the binary takes one for each
//! HTTP request while it is handled, and for each AMQP connection while it is
//! open. The emulator is idle once nothing holds a guard and the last one was
//! dropped (or taken) at least the timeout ago.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Activity {
    last: Mutex<Instant>,
    busy: AtomicUsize,
}

/// Marks the emulator busy until dropped.
#[derive(Debug)]
pub struct Busy(Arc<Activity>);

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { last: Mutex::new(Instant::now()), busy: AtomicUsize::new(0) })
    }

    fn touch(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Marks the start of some activity; it lasts until the guard is dropped.
    pub fn begin(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Busy(self.clone())
    }

    /// How long the emulator has been idle: zero while anything is busy.
    pub fn idle(&self) -> Duration {
        if self.busy.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        self.last.lock().unwrap_or_else(|e| e.into_inner()).elapsed()
    }

    /// Resolves once the emulator has been idle for `timeout`.
    pub async fn idle_for(&self, timeout: Duration) {
        loop {
            let idle = self.idle();
            if idle >= timeout {
                return;
            }
            // Busy: look again soon. Idle: sleep until the timeout would pass.
            let wait = if self.busy.load(Ordering::SeqCst) > 0 { timeout } else { timeout - idle };
            tokio::time::sleep(wait.clamp(Duration::from_millis(10), Duration::from_secs(1))).await;
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_only_once_nothing_is_busy_for_the_timeout() {
        let activity = Activity::new();
        let timeout = Duration::from_millis(150);
        let busy = activity.begin();
        tokio::time::sleep(timeout * 2).await;
        assert_eq!(activity.idle(), Duration::ZERO, "a held guard keeps it busy");
        drop(busy);
        assert!(activity.idle() < timeout, "the end of activity counts as activity");
        let started = Instant::now();
        activity.idle_for(timeout).await;
        assert!(started.elapsed() >= timeout - Duration::from_millis(20));
        assert!(activity.idle() >= timeout);
    }
}
