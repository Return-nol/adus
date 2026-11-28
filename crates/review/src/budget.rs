//! Per-review input-token ceiling (`settings.max_review_input_tokens`). One [`TokenBudget`] is
//! shared by every Finder, Sharded Rules, Verifier and post-verify call of a review run. It is
//! a soft cap: calls already in flight finish, each running agent may send one more submit-only
//! call, and the Verifier still checks the remaining candidates diff-only, so a review may pass
//! the limit.
//!
//! It also counts what the ceiling changed inside the stages (agent runs that wrapped up early,
//! candidates verified diff-only) so the summary names it only when something was cut.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// A prompt-cache read costs a quarter of an uncached input token in the budget.
pub(crate) const CACHE_READ_DIVISOR: u32 = 4;

/// Input tokens one review run has sent, against its limit; cache reads are weighted by
/// `CACHE_READ_DIVISOR`.
#[derive(Debug)]
pub(crate) struct TokenBudget {
    limit: u64,
    used: AtomicU64,
    /// Input tokens sent, unweighted (cache reads in full), for the stage log.
    raw_input: AtomicU64,
    /// Input tokens read from the prompt cache, for the stage log.
    cache_read: AtomicU64,
    /// Input tokens written to the prompt cache, for the stage log.
    cache_write: AtomicU64,
    /// Agent runs (Finder, Verifier or rule evidence collector) that wrapped up early because
    /// the budget was exhausted (budget-driven wrap-ups only).
    wrap_ups: AtomicUsize,
    /// Verifier candidates checked diff-only instead of with the repository tools.
    diff_only_verifications: AtomicUsize,
}

// Relaxed is enough everywhere: the cap is soft, the counters are only read once the stages
// that write them have finished, and nothing else is ordered by them.
impl TokenBudget {
    /// `limit` 0 means no limit: the budget is never exhausted.
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
            raw_input: AtomicU64::new(0),
            cache_read: AtomicU64::new(0),
            cache_write: AtomicU64::new(0),
            wrap_ups: AtomicUsize::new(0),
            diff_only_verifications: AtomicUsize::new(0),
        }
    }

    /// Counts the input tokens of one response received. Tokens read from the prompt cache
    /// (clamped to `input_tokens`) are charged at `1 / CACHE_READ_DIVISOR`, rounded up; cache
    /// writes are charged as part of `input_tokens`. The raw input and the cache reads and
    /// writes are also summed as they are, for the stage log, whatever the limit.
    pub(crate) fn add(&self, input_tokens: u32, cache_read_tokens: u32, cache_write_tokens: u32) {
        let cached = cache_read_tokens.min(input_tokens);
        let uncached = input_tokens - cached;
        let charged = uncached.saturating_add(cached.div_ceil(CACHE_READ_DIVISOR));
        self.used.fetch_add(u64::from(charged), Ordering::Relaxed);
        self.raw_input
            .fetch_add(u64::from(input_tokens), Ordering::Relaxed);
        self.cache_read
            .fetch_add(u64::from(cache_read_tokens), Ordering::Relaxed);
        self.cache_write
            .fetch_add(u64::from(cache_write_tokens), Ordering::Relaxed);
    }

    /// The weighted budget left as a percentage of the limit; `100` when unlimited.
    pub(crate) fn remaining_share_percent(&self) -> u32 {
        if self.limit == 0 {
            return 100;
        }
        let left = self.limit.saturating_sub(self.used());
        // `left <= limit`, so the percentage is at most 100.
        u32::try_from(u128::from(left) * 100 / u128::from(self.limit)).unwrap_or(100)
    }

    /// Weighted input tokens counted so far (what the limit is checked against).
    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Input tokens sent so far, cache reads counted in full.
    pub(crate) fn raw_input(&self) -> u64 {
        self.raw_input.load(Ordering::Relaxed)
    }

    /// Input tokens read from the prompt cache so far.
    pub(crate) fn cache_read(&self) -> u64 {
        self.cache_read.load(Ordering::Relaxed)
    }

    /// Input tokens written to the prompt cache so far.
    pub(crate) fn cache_write(&self) -> u64 {
        self.cache_write.load(Ordering::Relaxed)
    }

    /// True once a non-zero limit is reached.
    pub(crate) fn exhausted(&self) -> bool {
        self.limit > 0 && self.used() >= self.limit
    }

    /// An agent run wrapped up early because the budget was exhausted.
    pub(crate) fn note_wrap_up(&self) {
        self.wrap_ups.fetch_add(1, Ordering::Relaxed);
    }

    /// A Verifier candidate was checked diff-only instead of with the repository tools.
    pub(crate) fn note_diff_only(&self) {
        self.diff_only_verifications.fetch_add(1, Ordering::Relaxed);
    }

    /// Agent runs that wrapped up early because of the budget, so far.
    pub(crate) fn wrap_ups(&self) -> usize {
        self.wrap_ups.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn diff_only_verifications(&self) -> usize {
        self.diff_only_verifications.load(Ordering::Relaxed)
    }

    /// The summary note, or `None` when the ceiling changed nothing. `batches_skipped` counts
    /// the Finder batches never started (taken from the Finder's final tasks, so a re-run
    /// without tools is not counted twice); `finder_wrap_ups` is [`Self::wrap_ups`] read when
    /// the Finder finished, so Verifier wrap-ups are not named; `rules_skipped` counts the
    /// rule shards and evidence loops never started.
    pub(crate) fn warning(
        &self,
        batches_skipped: usize,
        finder_wrap_ups: usize,
        rules_skipped: usize,
    ) -> Option<String> {
        let mut details = Vec::new();
        match batches_skipped {
            0 => {}
            1 => details.push("1 batch not reviewed".to_string()),
            n => details.push(format!("{n} batches not reviewed")),
        }
        if finder_wrap_ups > 0 {
            details.push("some reviewers stopped early".to_string());
        }
        if rules_skipped > 0 {
            details.push("team rules partly unchecked".to_string());
        }
        match self.diff_only_verifications.load(Ordering::Relaxed) {
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
    fn token_budget_exhausts_only_at_non_zero_limit() {
        let zero = TokenBudget::new(0);
        assert!(!zero.exhausted());
        zero.add(u32::MAX, 0, 0);
        zero.add(u32::MAX, 0, 0);
        assert_eq!(zero.used(), 2 * u64::from(u32::MAX));
        assert!(!zero.exhausted());

        let budget = TokenBudget::new(1_000);
        budget.add(600, 0, 0);
        budget.add(399, 0, 0);
        assert_eq!(budget.used(), 999);
        assert!(!budget.exhausted());
        budget.add(1, 0, 0);
        assert_eq!(budget.used(), 1_000);
        assert!(budget.exhausted(), "reaching the limit exhausts it");
        budget.add(500, 0, 0);
        assert!(budget.exhausted());
    }

    #[test]
    fn token_budget_discounts_cache_reads_by_quarter() {
        let budget = TokenBudget::new(0);
        budget.add(1_000, 0, 0);
        assert_eq!(budget.used(), 1_000);
        // 200 uncached + 800 / 4 cached.
        budget.add(1_000, 800, 0);
        assert_eq!(budget.used(), 1_400);
        // Cache reads are clamped to the input: 100 / 4.
        budget.add(100, 500, 0);
        assert_eq!(budget.used(), 1_425);
        // 5 cached tokens round up to 2.
        budget.add(5, 5, 0);
        assert_eq!(budget.used(), 1_427);
        // The raw total counts every input token in full; cache reads and writes are summed
        // as reported (writes do not change the weighting).
        budget.add(10, 0, 7);
        assert_eq!(budget.used(), 1_437);
        assert_eq!(budget.raw_input(), 2_115);
        assert_eq!((budget.cache_read(), budget.cache_write()), (1_305, 7));
    }

    #[test]
    fn remaining_share_percent_tracks_the_weighted_budget() {
        let unlimited = TokenBudget::new(0);
        unlimited.add(1_000_000, 0, 0);
        assert_eq!(unlimited.remaining_share_percent(), 100);

        let budget = TokenBudget::new(1_000);
        assert_eq!(budget.remaining_share_percent(), 100);
        // 1_000 cached tokens weigh 250: a quarter of the limit.
        budget.add(1_000, 1_000, 0);
        assert_eq!(budget.remaining_share_percent(), 75);
        budget.add(5_000, 0, 0);
        assert_eq!(budget.remaining_share_percent(), 0);
    }

    #[test]
    fn warning_only_when_something_was_cut() {
        let budget = TokenBudget::new(10);
        budget.add(12, 0, 0);
        assert!(budget.exhausted());
        // Crossed, but nothing was skipped, stopped or switched: no warning.
        assert_eq!(budget.warning(0, 0, 0), None);

        budget.note_diff_only();
        assert_eq!(
            budget.warning(0, 0, 0).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 1 finding verified from the diff only."
            )
        );

        budget.note_diff_only();
        budget.note_wrap_up();
        assert_eq!(budget.wrap_ups(), 1);
        assert_eq!(budget.diff_only_verifications(), 2);
        assert_eq!(
            budget.warning(2, 1, 3).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 2 batches not reviewed, some reviewers stopped early, team rules partly unchecked, 2 findings verified from the diff only."
            )
        );
        // Only the Finder's wrap-ups (the count the caller passes) are named.
        assert_eq!(
            budget.warning(2, 0, 0).as_deref(),
            Some(
                "Token budget reached (12 of 10 input tokens): 2 batches not reviewed, 2 findings verified from the diff only."
            )
        );

        let one = TokenBudget::new(1);
        one.add(1, 0, 0);
        assert_eq!(
            one.warning(1, 0, 0).as_deref(),
            Some("Token budget reached (1 of 1 input tokens): 1 batch not reviewed.")
        );
        // Rule work skipped for the budget is named on its own.
        assert_eq!(
            one.warning(0, 0, 1).as_deref(),
            Some("Token budget reached (1 of 1 input tokens): team rules partly unchecked.")
        );
    }
}
