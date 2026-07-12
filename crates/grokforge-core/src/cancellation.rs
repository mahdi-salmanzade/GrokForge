//! Cooperative cancellation for a single agent turn.

use tokio_util::sync::CancellationToken;

/// A clonable handle used by frontends to request that an active turn stop safely.
///
/// Cancellation is cooperative: network and sandbox work stop promptly, while already-running
/// host-process mutations are awaited before the turn returns and releases its rollout lock.
#[derive(Clone, Debug, Default)]
pub struct TurnCancellation {
    token: CancellationToken,
}

impl TurnCancellation {
    /// Create a fresh, independent turn cancellation handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Calling this more than once is harmless.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Whether cancellation has already been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    pub(crate) fn process_token(&self) -> CancellationToken {
        self.token.clone()
    }
}
