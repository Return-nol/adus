//! Per-review input-token ceiling (`settings.max_review_input_tokens`). One [`TokenBudget`] is
//! shared by every Finder, Verifier and Dedup call of a review run. It is a soft cap: calls
//! already in flight finish, so a review may pass the limit by those calls.

use std::sync::atomic::{AtomicU64, Ordering};

/// Input tokens (cached included) one review run has sent, against its limit.
#[derive(Debug)]
pub struct TokenBudget {
    limit: u64,
    used: AtomicU64,
}

impl TokenBudget {
    /// `limit` 0 means no limit: the budget is never exhausted.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    /// Counts the input tokens of one response received.
    pub fn add(&self, input_tokens: u32) {
        // Relaxed is enough: the cap is soft and nothing else is ordered by this counter.
        self.used
            .fetch_add(u64::from(input_tokens), Ordering::Relaxed);
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// True once a non-zero limit is reached.
    pub fn exhausted(&self) -> bool {
        self.limit > 0 && self.used() >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_zero_never_exhausts() {
        let budget = TokenBudget::new(0);
        assert!(!budget.exhausted());
        budget.add(u32::MAX);
        budget.add(u32::MAX);
        assert_eq!(budget.used(), 2 * u64::from(u32::MAX));
        assert_eq!(budget.limit(), 0);
        assert!(!budget.exhausted());
    }

    #[test]
    fn token_budget_exhausts_at_limit() {
        let budget = TokenBudget::new(1_000);
        assert_eq!(budget.limit(), 1_000);
        budget.add(600);
        assert!(!budget.exhausted());
        budget.add(399);
        assert_eq!(budget.used(), 999);
        assert!(!budget.exhausted());
        budget.add(1);
        assert_eq!(budget.used(), 1_000);
        assert!(budget.exhausted(), "reaching the limit exhausts it");
        budget.add(500);
        assert!(budget.exhausted());
    }
}
