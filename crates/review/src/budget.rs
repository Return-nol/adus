//! Per-review input-token ceiling (`settings.max_review_input_tokens`). One [`TokenBudget`] is
//! shared by every Finder, Verifier and Dedup call of a review run. It is a soft cap: calls
//! already in flight finish, each running agent may send one more submit-only call, and the
//! Verifier still checks the remaining candidates diff-only, so a review may pass the limit.
//!
//! It also counts what the ceiling changed inside the stages (agent runs that wrapped up early,
//! candidates verified diff-only) so the summary names it only when something was cut.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Input tokens (cached included) one review run has sent, against its limit.
#[derive(Debug)]
pub struct TokenBudget {
    limit: u64,
    used: AtomicU64,
    /// Agent runs (Finder or Verifier) that wrapped up early because of the budget.
    wrap_ups: AtomicUsize,
    /// Verifier candidates checked diff-only instead of with the repository tools.
    diff_only_verifications: AtomicUsize,
}

// Relaxed is enough everywhere: the cap is soft, the counters are only read once the stages
// that write them have finished, and nothing else is ordered by them.
impl TokenBudget {
    /// `limit` 0 means no limit: the budget is never exhausted.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
            wrap_ups: AtomicUsize::new(0),
            diff_only_verifications: AtomicUsize::new(0),
        }
    }

    /// Counts the input tokens of one response received.
    pub fn add(&self, input_tokens: u32) {
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

    /// An agent run wrapped up early because the budget was exhausted.
    pub fn note_wrap_up(&self) {
        self.wrap_ups.fetch_add(1, Ordering::Relaxed);
    }

    /// A Verifier candidate was checked diff-only instead of with the repository tools.
    pub fn note_diff_only(&self) {
        self.diff_only_verifications.fetch_add(1, Ordering::Relaxed);
    }

    /// Agent runs that wrapped up early because of the budget, so far.
    pub fn wrap_ups(&self) -> usize {
        self.wrap_ups.load(Ordering::Relaxed)
    }

    pub fn diff_only_verifications(&self) -> usize {
        self.diff_only_verifications.load(Ordering::Relaxed)
    }

    /// The summary note, or `None` when the ceiling changed nothing. `batches_skipped` counts
    /// the Finder batches never started (taken from the Finder's final tasks, so a re-run
    /// without tools is not counted twice); `finder_wrap_ups` is [`Self::wrap_ups`] read when
    /// the Finder finished, so Verifier wrap-ups are not named.
    pub fn warning(&self, batches_skipped: usize, finder_wrap_ups: usize) -> Option<String> {
        let mut details = Vec::new();
        match batches_skipped {
            0 => {}
            1 => details.push("1 batch not reviewed".to_string()),
            n => details.push(format!("{n} batches not reviewed")),
        }
        if finder_wrap_ups > 0 {
            details.push("some reviewers stopped early".to_string());
        }
        match self.diff_only_verifications() {
            0 => {}
            1 => details.push("1 finding verified from the diff only".to_string()),
            n => details.push(format!("{n} findings verified from the diff only")),
        }
        if details.is_empty() {
            return None;
        }
        Some(format!(
            "Token budget reached ({} of {} input tokens): {}.",
            self.used(),
            self.limit,
            details.join(", ")
        ))
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

    #[test]
    fn warning_only_when_something_was_cut() {
        let budget = TokenBudget::new(10);
        budget.add(12);
        assert!(budget.exhausted());
        // Crossed, but nothing was skipped, stopped or switched: no warning.
        assert_eq!(budget.warning(0, 0), None);

        budget.note_diff_only();
        assert_eq!(
            budget.warning(0, 0).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 1 finding verified from the diff only."
            )
        );

        budget.note_diff_only();
        budget.note_wrap_up();
        assert_eq!(budget.wrap_ups(), 1);
        assert_eq!(budget.diff_only_verifications(), 2);
        assert_eq!(
            budget.warning(2, 1).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 2 batches not reviewed, some reviewers stopped early, 2 findings verified from the diff only."
            )
        );
        // Only the Finder's wrap-ups (the count the caller passes) are named.
        assert_eq!(
            budget.warning(2, 0).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 2 batches not reviewed, 2 findings verified from the diff only."
            )
        );

        let one = TokenBudget::new(1);
        one.add(1);
        assert_eq!(
            one.warning(1, 0).as_deref(),
            Some("Token budget reached (1 of 1 input tokens): 1 batch not reviewed.")
        );
    }
}
