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
            (self.cached_tokens as f64 / self.input_tokens as f64).min(1.0)
        }
    }

    /// Accumulate another request's usage into this running total.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(other.cached_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_ratio_stays_within_documented_range() {
        assert!(Usage::default().cache_hit_ratio().abs() < f64::EPSILON);
        let clamped = Usage {
            input_tokens: 10,
            cached_tokens: 50,
            ..Usage::default()
        }
        .cache_hit_ratio();
        assert!((clamped - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn accumulating_untrusted_usage_counters_saturates() {
        let mut total = Usage {
            input_tokens: u64::MAX,
            cached_tokens: u64::MAX,
            output_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };
        total.add(Usage {
            input_tokens: 1,
            cached_tokens: 1,
            output_tokens: 1,
            reasoning_tokens: 1,
        });
        assert_eq!(
            total,
            Usage {
                input_tokens: u64::MAX,
                cached_tokens: u64::MAX,
                output_tokens: u64::MAX,
                reasoning_tokens: u64::MAX,
            }
        );
    }
}
