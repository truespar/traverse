//! Cooperative cancellation for long training loops.
//!
//! A `CancellationToken` is a cheap `Arc<AtomicBool>` handle the loop polls
//! between epochs and mini-batches; external callers (a job's cancel request, a
//! Ctrl-C handler) hold a clone and flip it to stop training.
//!
//! - **Caller checks.** [`CancellationToken::with_check`] adds a function polled
//!   along with the flag: how a Cypher procedure hands its query's deadline,
//!   stop flag and memory ceiling to the training loop.
//! - **Default = never cancelled.** `CancellationToken::none()` stays false
//!   forever, so callers that don't care needn't wrap every train call in an
//!   `Option`.
//! - **Clones share state**: `clone()` copies the `Arc`, so canceller and
//!   poller see the same flag.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

type Check = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
    check: Option<Check>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.flag.load(Ordering::Relaxed))
            .field("has_check", &self.check.is_some())
            .finish()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::none()
    }
}

impl CancellationToken {
    /// New token, not cancelled. Clones share state: `cancel()` on any trips all.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            check: None,
        }
    }

    /// Also cancelled whenever `check` returns true; it runs on whichever thread polls.
    pub fn with_check(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            check: Some(Arc::new(check)),
            ..Self::new()
        }
    }

    /// For call sites that won't cancel. Same as [`Self::new`]; the name tells readers `.cancel()`
    /// won't be called.
    pub fn none() -> Self {
        Self::new()
    }

    /// Trips this token and every clone. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Polls the flag (an atomic load, ~ns) and the check, if any. Training loops check at epoch
    /// and mini-batch boundaries.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed) || self.check.as_ref().is_some_and(|c| c())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_is_not_cancelled() {
        let tok = CancellationToken::new();
        assert!(!tok.is_cancelled());
    }

    #[test]
    fn cancel_flips_the_flag() {
        let tok = CancellationToken::new();
        assert!(!tok.is_cancelled());
        tok.cancel();
        assert!(tok.is_cancelled());
    }

    #[test]
    fn clones_share_state() {
        let tok = CancellationToken::new();
        let alias = tok.clone();
        assert!(!tok.is_cancelled());
        assert!(!alias.is_cancelled());

        // cancel via the clone; the original sees it
        alias.cancel();
        assert!(tok.is_cancelled());
        assert!(alias.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let tok = CancellationToken::new();
        tok.cancel();
        tok.cancel();
        tok.cancel();
        assert!(tok.is_cancelled());
    }

    #[test]
    fn a_check_cancels_too() {
        fn always() -> bool {
            true
        }
        fn never() -> bool {
            false
        }
        assert!(CancellationToken::with_check(always).is_cancelled());
        let tok = CancellationToken::with_check(never);
        assert!(!tok.is_cancelled());
        tok.cancel();
        assert!(tok.is_cancelled());
    }

    #[test]
    fn cross_thread_cancellation() {
        // another thread flips the flag, the training-loop thread polls and sees it
        let tok = CancellationToken::new();
        let canceller = tok.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            canceller.cancel();
        });

        let start = std::time::Instant::now();
        while !tok.is_cancelled() {
            if start.elapsed() > std::time::Duration::from_secs(1) {
                panic!("cancellation never propagated across thread");
            }
            std::thread::yield_now();
        }
        handle.join().unwrap();
    }
}
