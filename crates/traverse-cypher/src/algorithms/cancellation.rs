//! Per-call cancellation token for algorithm kernels. Kernels call [`CancellationToken::check`] at
//! iteration boundaries (once per outer iteration, or every N inner iterations in tight loops). The
//! token holds the running query, so its deadline, a cancel or terminate, and the process memory
//! ceiling stop kernels on rayon workers too, where the query isn't the thread's current one.

use std::sync::Arc;

use super::AlgorithmError;
use crate::query::QueryContext;

/// Cancellation handle passed to each algorithm `run()`; cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    query: Option<Arc<QueryContext>>,
}

impl CancellationToken {
    /// A token no query stops (unit tests, direct API callers outside a query); the process memory
    /// ceiling still applies.
    pub fn none() -> Self {
        Self { query: None }
    }

    /// Follows the query running on this thread (none outside a query).
    pub fn current() -> Self {
        Self {
            query: crate::query::current(),
        }
    }

    /// Follows `query`.
    pub fn for_query(query: Arc<QueryContext>) -> Self {
        Self { query: Some(query) }
    }

    /// Has the query stopped, or memory run out? For parallel loops that can't return an error:
    /// skip the remaining items, then [`check`](Self::check) after the loop.
    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.query.as_ref().is_some_and(|q| q.interrupt().is_some())
            || crate::query::is_memory_exhausted()
    }

    /// Error if the query stopped or memory is exhausted. ~ns otherwise: a relaxed load, a clock
    /// read when there's a deadline, and a cached RSS comparison.
    #[inline]
    pub fn check(&self) -> Result<(), AlgorithmError> {
        if let Some(query) = &self.query
            && let Some(reason) = query.interrupt()
        {
            return Err(AlgorithmError::Interrupted(reason));
        }
        if crate::query::is_memory_exhausted() {
            return Err(AlgorithmError::MemoryExhausted);
        }
        Ok(())
    }
}
