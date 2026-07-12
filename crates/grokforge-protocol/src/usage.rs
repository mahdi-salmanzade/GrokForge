//! Token usage and cost, surfaced in the status line and cost display.

use serde::{Deserialize, Serialize};

/// Token accounting for one request, including the cache-hit and reasoning breakdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Fraction of input tokens served from the prompt cache (0.0–1.0). Precision loss on
    /// the cast is irrelevant for a displayed ratio.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 / self.input_tokens as f64
        }
    }

    /// Accumulate another request's usage into this running total.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

/// Why a turn stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a final answer.
    EndTurn,
    /// The user interrupted.
    Interrupted,
    /// The tool-iteration cap was hit.
    MaxIterations,
    /// An unrecoverable error ended the turn.
    Error,
}
