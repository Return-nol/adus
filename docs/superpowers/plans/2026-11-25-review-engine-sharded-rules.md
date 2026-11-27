# Review Engine Sharded Rules Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Adus's agentic rule checking with a deterministic per-file Sharded Rules Judge and per-candidate Evidence Collector/Judge running in parallel with the Finder.

**Architecture:** When `toolbox.is_some()` and enabled `<rules>` exist, `engine.rs` runs `run_sharded_rules` concurrently with `run_finder` under `deadlines.finder`. Each changed file with matching rules (`rule_matches_path`) gets one structured `fast` @ `High` shard call (with diff + capped head file content); each candidate violation triggers a 6-step `fast` @ `High` Evidence Collector (`read_file`/`grep`/`list_dir`) followed by a deterministic `read_contains_line`-gated Evidence Judge. Verified rule findings carry `severity: Some(rule.severity)` and skip the general Verifier straight into Semantic Dedup → Enrichment.

**Tech Stack:** Rust (tokio, futures stream, serde_json), `crates/review`, `adus_db::rules::Rule`, `adus_git::FileDiff`, sqlx Postgres + Redis (`crates/review/tests/engine.rs`).

**Spec:** [docs/superpowers/specs/2026-11-25-review-engine-overhaul-design.md](../specs/2026-11-25-review-engine-overhaul-design.md)

**Prerequisite:** Land [2026-11-25-review-engine-core-loop.md](2026-11-25-review-engine-core-loop.md) first (provides `AgentConfig::no_tools_on_last_step`, `Suggestion { severity: Option<Severity>, category: Option<FindingCategory>, .. }`, `classify_severity`, `enrich_suggestions`, and `Deadlines { finder, verifier, post_verify }`), **and** its Task 10 live acceptance run on better-booru PR #13 must have passed or had its failures triaged (spec §9.2). Do not start Task 1 here otherwise.

## Global Constraints

- **Clean-room and no-assumption rule (binding, verbatim from `AGENTS.md`):**
  1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. **Order of authority:** the task brief (or active unmerged change spec), then [docs/SPEC.md](docs/SPEC.md) (and [DESIGN.md](docs/DESIGN.md) for `web/`), then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. **Openness:** every implementer report lists its assumptions under an `"Assumptions"` heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. **Reviewers** flag as **Important** any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing [docs/SPEC.md](docs/SPEC.md) updates when SDD completes.
  5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
- **Model & Effort Matrix:** `fast` @ `ReasoningEffort::High` for Sharded Rule Judge, Rule Evidence Collector, and Rule Evidence Judge (user decision 2026-11-26; spec §2). The client adds the `High` reasoning reserve on top of each call's `max_tokens`.
- **Exact Constants:**
  - `SHARD_CONCURRENCY = 4`, `SHARD_MAX_TOKENS = 2_048`
  - `RULE_FILE_MAX_LINES = 600`, `RULE_FILE_MAX_CHARS = 30_000`
  - `RULE_EVIDENCE_CONCURRENCY = 4`, `RULE_EVIDENCE_MAX_STEPS = 6`, `MAX_RULE_EVIDENCE_CHARS = 24_000`, `RULE_RELATED_PATCH_CHARS = 8_000`, `RULE_JUDGE_MAX_TOKENS = 1_024`
  - `RULE_EVIDENCE_DEADLINE_RESERVE = Duration::from_secs(10)`
- **No dead code or stale tests (user requirement, 2026-11-26):** every task deletes, in the same commit, whatever its change makes obsolete: functions, constants, struct fields, enum variants (including everything left behind by `SpecialistKind::Rules`), parameters, `#[allow(dead_code)]` escapes, prompt text, test helpers, fakes, fixtures, and tests that pin removed behaviour (rewrite a test that still covers a live behaviour instead of deleting it). No `#[allow(dead_code)]` or `_`-prefixed rename may be added to silence a warning. Doc comments that describe removed behaviour are fixed in the same commit. The task report lists what was deleted.
- **Standing Rules:** Zero `.unwrap()`/`.expect()` outside tests; no new `dyn Trait`; `spawn_blocking` for filesystem reads; redact secrets before prompting/logging; diff-only / `small_pr` / no-clone runs keep `<rules>` in `Generalist` prompt as fallback.
- **Verification Commands:** `cargo test -p adus-review -j 2`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`.
- **Commits:** Conventional Commits; never push. **Never commit `.cargo/config.toml`** (local edit holding secrets): before every commit run `git diff --cached --name-only` and confirm it is absent, and every `git commit` here commits only the listed paths (`git commit -m "<msg>" -- <paths>`).
- **Tool-enabled Finder prompts omit the `<rules>` section entirely** (spec §7: rules are no longer put into tool-enabled `Generalist`); diff-only / `small_pr` / no-clone prompts keep it.

## Review Focus

1. **Off-shape shard reply & stringified line numbers:** A model answering a bare `[]`, `[{...}]`, `{"findings": [...]}`, or `"line": "42"` must parse cleanly, while out-of-range `rule_id` (`0` or `> rules.len()`) or a `line` outside every hunk is dropped without poisoning valid violations in the same shard, and a violation on a context line moves to the nearest `+` line of its hunk instead of being lost. Pinned in Task 1 (`lenient_shard_parser_handles_bare_array_string_lines_and_invalid_anchors`).
2. **Oversized or `Added` head file inlining:** `Added` files (already complete in their diff) and files exceeding `RULE_FILE_MAX_LINES (600)` or `RULE_FILE_MAX_CHARS (30_000)` omit `<file_content>` and judge from diff + rules without failing. Pinned in Task 1 (`read_head_file_capped_skips_added_and_oversized_files`).
3. **Unverified citation / fabricated `path:line`:** If the Evidence Judge returns `verdict: "violated"` citing a `path:line` not present in the collector's `read_file` excerpts (`!read_contains_line`), the violation is withheld. Pinned in Task 2 (`evidence_judge_rejects_violated_verdict_without_excerpt_line`).
4. **High-confidence source refutation short-circuit:** If the Evidence Collector returns `keep: false, confidence: "high"` citing a `path:line` verified in its collected excerpts, drop immediately without calling the Evidence Judge. Pinned in Task 2 (`collector_high_confidence_refutation_skips_judge`).
5. **Diff-only / `small_pr` fallback keeps rules active:** When `toolbox.is_none()` (small PR or failed clone), Sharded Rules is skipped and `<rules>` remains in the `Generalist` user prompt; when `toolbox.is_some()`, `<rules>` is omitted from tool-enabled Finder prompts and `SpecialistKind::Rules` is not spawned. Pinned in Task 3 (`tool_enabled_finder_omits_rules_while_diff_only_keeps_them`).

---

### Task 1: Per-File Sharded Rule Prompt Builder, Capped Head Reader & Lenient Wire Parser

**Files:**
- Create: `crates/review/src/rules_judge.rs` (prompt builder, wire parser, `ShardViolation` types)
- Modify: `crates/review/src/tools.rs` (`Toolbox::read_head_file_capped`)
- Modify: `crates/review/src/lib.rs` (add `mod rules_judge;`)

**Interfaces:**
- Consumes: `diff_block`, `wrap`, `redact_secrets` (`prompt.rs`/`redact.rs`); `CommentableRange` (`parse.rs`); `adus_db::rules::{Rule, rule_matches_path}`.
- Produces:
  - `Toolbox::read_head_file_capped(&self, rel_path: &str, max_lines: usize, max_chars: usize) -> Option<String>` in `crates/review/src/tools.rs` (uses `spawn_blocking`, `resolve_existing_file`, `read_text`, redacts secrets, returns `None` if binary, unreadable, empty, `lines > max_lines`, or `chars > max_chars`; formats lines with `{n}: {line}` numbering).
  - `pub(crate) struct RawShardViolation { pub rule_index: usize, pub line: u32, pub start_line: Option<u32>, pub claim: String }`
  - `pub(crate) fn build_shard_prompts(file: &FileDiff, matching_rules: &[&Rule], head_content: Option<&str>) -> (String, String)` in `crates/review/src/rules_judge.rs` (numbers rules `[1]..[n]`, wraps diff and optional `<file_content>`).
  - `pub(crate) fn parse_shard_response(raw: &str, rule_count: usize, range: &CommentableRange, hunks: &[(u32, u32)]) -> Vec<RawShardViolation>` in `crates/review/src/rules_judge.rs` (`hunks` = the file's right-side hunk ranges from `adus_git::diff::commentable_hunks`, context lines included). Accepts `{"violations": [...]}` or bare `[...]` or `{"findings": [...]}`, coerces numeric string `line` / `rule_id`, drops `rule_id == 0 || rule_id > rule_count`. Anchoring (spec §7.2): a `line` in `range.lines` (a `+` line) is kept with its `start_line` if `same_hunk` holds, else `start_line` is dropped; a `line` inside a hunk but not a `+` line moves to the nearest `+` line of that hunk (smallest distance, earlier line on a tie), `start_line` dropped, and `RawShardViolation.cited_line: Option<u32>` keeps the original line so the posted body can say where the problem is; a `line` outside every hunk, or in a hunk with no `+` line, is dropped.
  - `RawShardViolation` gains `pub cited_line: Option<u32>` (`Some(original)` only when the line was moved).

- [ ] **Step 1: Write failing tests in `tools.rs` and `rules_judge.rs`**

- `tools.rs`: `read_head_file_capped_skips_added_and_oversized_files` (verifies within-limit file returns `{n}: ...` numbered secret-redacted text, while >600 lines or >30,000 chars returns `None`).
- `rules_judge.rs`: `lenient_shard_parser_handles_bare_array_string_lines_and_invalid_anchors` (tests `{"violations": [...]}`, bare `[{...}]`, empty `[]`, `{"findings": [...]}`, stringified `"line": "3"`, `rule_id: 0`, `rule_id: 99`, off-diff `line: 999` (dropped), a context line 2 lines below the only `+` line of its hunk (moved to that `+` line, `cited_line == Some(original)`, `start_line == None`), a context line equidistant from two `+` lines (moved to the earlier), and a hunk with no `+` line (dropped)).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- read_head_file_capped lenient_shard_parser`
Expected: FAIL.

- [ ] **Step 3: Implement `Toolbox::read_head_file_capped` in `tools.rs` and `build_shard_prompts` + `parse_shard_response` in `rules_judge.rs`**

Register `mod rules_judge;` in `crates/review/src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib -- read_head_file_capped lenient_shard_parser`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git diff --cached --name-only   # must not list .cargo/config.toml
git add crates/review/src/rules_judge.rs crates/review/src/tools.rs crates/review/src/lib.rs
git commit -m "feat(review): add per-file sharded rule prompt builder, head reader, and lenient parser" -- crates/review/src/rules_judge.rs crates/review/src/tools.rs crates/review/src/lib.rs
```

---

### Task 2: Per-Candidate Rule Evidence Collector & Separate Evidence Judge

**Files:**
- Create: `crates/review/src/rules_evidence.rs`
- Modify: `crates/review/src/lib.rs` (add `mod rules_evidence;`)

**Interfaces:**
- Consumes: `run_react_loop`, `chat_single_turn`, `AgentConfig::no_tools_on_last_step`, `SubmitSpec` (`agent.rs`); `Toolbox` (`tools.rs`); `TokenBudget` (`budget.rs`); `RawShardViolation` (`rules_judge.rs`).
- Produces:
  - `pub(crate) struct ReadExcerpt { pub path: String, pub content: String }`
  - `pub(crate) fn read_contains_line(excerpts: &[ReadExcerpt], path: &str, line: u32) -> bool` (matches normalized `path` and checks any line in `content` starts with `"{line}: "` with non-empty text after prefix).
  - `pub(crate) fn has_source_refutation(keep: bool, confidence: Option<&str>, rationale: &str, excerpts: &[ReadExcerpt]) -> bool` (returns `true` only when `!keep && confidence == Some("high")` and `rationale` cites `{excerpt.path}:{line}` matching `read_contains_line`).
  - `pub(crate) enum EvidenceVerdict { Verified { path: String, line: u32, rationale: String }, Refuted, UnsupportedCitation, Rejected(&'static str) }`
  - `pub(crate) async fn verify_rule_candidate(ctx: RuleEvidenceContext<'_>, rule: &Rule, file: &FileDiff, candidate: &RawShardViolation, all_files: &[FileDiff], deadline: Instant) -> EvidenceVerdict`
    - Step A: Runs 6-step `run_react_loop` on `fast_model` @ `ReasoningEffort::High` (`no_tools_on_last_step: true`, `budget_bands: false`), capturing successful `read_file` outputs (`!result.is_error`, numbered lines `^\d+: `) into `Vec<ReadExcerpt>` (also add a lightweight `AgentOutcome::tool_excerpts` or `Toolbox::excerpts` callback/capture — specifically `AgentOutcome::read_outputs: Vec<(String, String)>` populated in `agent.rs` from successful `read_file` results so `rules_evidence.rs` inspects the exact lines the collector read without changing `Toolbox`).
    - Short-circuits to `EvidenceVerdict::Refuted` if `has_source_refutation(...)` holds.
    - Step B: Deduplicates excerpts within `MAX_RULE_EVIDENCE_CHARS = 24_000` (fails closed with `Rejected("no_excerpts")` if empty), gathers related patches from `all_files` capped at `RULE_RELATED_PATCH_CHARS = 8_000`, calls `chat_single_turn` (with `fast_model`, `ReasoningEffort::High`, `RULE_JUDGE_MAX_TOKENS = 1_024`), parses `{ verdict, path, line, rationale }`, and returns `Verified` ONLY when `verdict == "violated"` AND `read_contains_line(&excerpts, &path, line)` is true; otherwise `UnsupportedCitation` or `Rejected(verdict)`.

- [ ] **Step 1: Write failing unit tests in `crates/review/src/rules_evidence.rs`**

Add tests with `ScriptedChat` + `RepoWorkspace::from_files`:
1. `evidence_judge_accepts_verified_citation_and_rejects_unverified`:
   - Case A: Collector reads `src/a.rs` (lines 1-10) and submits `{keep: true, ...}`; Judge returns `{"verdict": "violated", "path": "src/a.rs", "line": 4, "rationale": "..."}` -> `Verified`.
   - Case B: Judge returns `{"verdict": "violated", "path": "src/other.rs", "line": 12, "rationale": "..."}` (never read in collector excerpts) -> `UnsupportedCitation`.
2. `collector_high_confidence_refutation_skips_judge`: Collector reads `src/a.rs` (line 4), returns `{"keep": false, "confidence": "high", "rationale": "Guard at src/a.rs:4 handles it"}` -> `Refuted` with only 2 LLM calls (0 Judge calls).
3. `evidence_judge_rejects_violated_verdict_without_excerpt_line`: `read_contains_line` rejects empty/whitespace-only lines (`"4:   "`), wrong path, and non-matching line numbers.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- evidence_judge collector_high_confidence`
Expected: FAIL.

- [ ] **Step 3: Add `read_outputs: Vec<(String, String)>` to `AgentOutcome` in `agent.rs` and implement `crates/review/src/rules_evidence.rs` + `lib.rs`**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git diff --cached --name-only   # must not list .cargo/config.toml
git add crates/review/src/rules_evidence.rs crates/review/src/agent.rs crates/review/src/lib.rs
git commit -m "feat(review): add 6-step rule evidence collector and citation-verified judge" -- crates/review/src/rules_evidence.rs crates/review/src/agent.rs crates/review/src/lib.rs
```

---

### Task 3: Parallel Sharded Rules Orchestrator & Engine Integration

**Files:**
- Modify: `crates/review/src/rules_judge.rs` (add `run_sharded_rules`, `ShardedRulesOutput`, `ShardedRulesContext`)
- Modify: `crates/review/src/specialists.rs` (remove `SpecialistKind::Rules` and its prompts; update `active_specialists(mode: ReviewMode)`; move repository-instruction checks to `Bug`: its Role checks the changed hunks against `<repository_instructions>` and allows `category: "repo_instruction"` — spec §7)
- Modify: `crates/review/src/prompt.rs` (omit `<rules>` from tool-enabled Finder prompt when sharded rules run; keep `<rules>` for diff-only fallback)
- Modify: `crates/review/src/finder.rs` (thread `with_tools` rule visibility cleanly)
- Modify: `crates/review/src/verifier.rs` (`CandidateFinding { pub needs_verify: bool, pub rule_id: Option<Uuid>, .. }`)
- Modify: `crates/review/src/engine.rs` (run `run_sharded_rules` in `tokio::join!` alongside `run_finder` when `toolbox.is_some() && !s.rules.is_empty()`; skip HV2 Verifier when `!candidate.needs_verify`; log `rule_shards`, `rule_candidates`, `rule_verified`)
- Modify: `crates/review/tests/engine.rs` (add integration test + update `Stage` detection for `RuleShard` / `RuleEvidenceCollector` / `RuleEvidenceJudge`)

**Interfaces:**
- Consumes: `verify_rule_candidate` (`rules_evidence.rs`), `Toolbox`, `TokenBudget`, `CandidateFinding`.
- Produces:
  - `pub(crate) struct ShardedRulesOutput { pub findings: Vec<CandidateFinding>, pub shards_run: usize, pub candidates: usize, pub verified: usize }`
  - `pub(crate) async fn run_sharded_rules(ctx: ShardedRulesContext<'_>, files: &[FileDiff], allowed: &HashMap<String, CommentableRange>, rules: &[Rule], deadline: Instant) -> ShardedRulesOutput`
    - Fans out matching per-file shards with `SHARD_CONCURRENCY = 4` (`tokio::time::timeout_at(deadline, ...)`).
    - Fans out `verify_rule_candidate` with `RULE_EVIDENCE_CONCURRENCY = 4` up to `deadline - RULE_EVIDENCE_DEADLINE_RESERVE (10s)`.
    - Produces `CandidateFinding` with `severity: Some(rule.severity)`, `category: None`, `needs_verify: false`, `rule_id: Some(rule.id)`, and `body` naming the rule title (`"Rule \"{title}\": {claim} (verified at {path}:{line}: {rationale})"`); when `cited_line` is `Some(n)`, the body says the problem is at line `n` (`"… (line {n}, outside the changed lines)"`) because the comment is posted on the nearest `+` line.

- [ ] **Step 1: Write failing unit & integration tests in `rules_judge.rs`, `specialists.rs`, and `crates/review/tests/engine.rs`**

- `specialists.rs`: update `active_specialists` tests to verify `ReviewMode::Specialists` returns `[Bug, Security, Performance]` (no `Rules`), and that the `Bug` specialist's prompt (system Role + user prompt) carries `<repository_instructions>` and the instruction to check changed hunks against them, with `repo_instruction` accepted as its finding category, while `Security` and `Performance` keep `<repository_instructions>` as context only.
- `rules_judge.rs`: `tool_enabled_finder_omits_rules_while_diff_only_keeps_them` (checks `build_finder_user_prompt` with `with_tools = true` contains no `<rules>` tag at all — and the system prompt's Scope no longer mentions `<rules>` for tool-enabled runs — while `with_tools = false` includes the matching rules) + `run_sharded_rules_evaluates_matching_files_and_skips_unmatched` (2 files, 1 rule with glob `"src/**"`: only 1 shard call runs, verified finding carries `severity: Some(rule.severity)` and `needs_verify: false`).
- `crates/review/tests/engine.rs`: `review_runs_sharded_rules_in_parallel_and_skips_general_verifier` — repo with an active rule matching `src/a.rs`, cloned `Generalist` review runs Finder + Sharded Rules in parallel, verified rule finding skips general Verifier, enters Dedup → Enrich, and posts with the rule's `Severity` and rule title in the body.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- run_sharded_rules review_runs_sharded_rules`
Expected: FAIL.

- [ ] **Step 3: Implement `run_sharded_rules` and integrate across `specialists.rs`, `prompt.rs`, `finder.rs`, `verifier.rs`, `engine.rs`, and `tests/engine.rs`**

- Partition `candidates` in `engine.rs` before `verify_candidates`: items with `needs_verify == true` go to `verify_candidates` (`primary` @ `High`); verified Sharded Rule items (`needs_verify == false`) merge into `verified` directly before `deduplicate_findings` → `classify_severity` (preserves their `Some(rule.severity)`) → `enrich_suggestions` → `check_fix`.

- [ ] **Step 4: Run full workspace tests, clippy, and fmt**

Run:
```sh
cargo test -p adus-review -j 2
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: PASS with 0 errors and 0 warnings.

- [ ] **Step 5: Commit**

```sh
git diff --cached --name-only   # must not list .cargo/config.toml
git add crates/review/src/rules_judge.rs crates/review/src/specialists.rs crates/review/src/prompt.rs crates/review/src/finder.rs crates/review/src/verifier.rs crates/review/src/engine.rs crates/review/tests/engine.rs
git commit -m "feat(review): run deterministic sharded team rules in parallel with Finder" -- crates/review/src/rules_judge.rs crates/review/src/specialists.rs crates/review/src/prompt.rs crates/review/src/finder.rs crates/review/src/verifier.rs crates/review/src/engine.rs crates/review/tests/engine.rs
```

---

### Task 4: Sync `docs/SPEC.md` and Mark Spec Implemented

**Files:**
- Modify: `docs/SPEC.md` (§8.4, §10)
- Modify: `docs/superpowers/specs/2026-11-25-review-engine-overhaul-design.md` (`Status: draft` → `Status: implemented`)

- [ ] **Step 1: Update `docs/SPEC.md` and mark design spec implemented**

- Document the parallel Sharded Rules pipeline (per-file shards under `RULE_FILE_MAX_LINES = 600` / `RULE_FILE_MAX_CHARS = 30_000`, 6-step Evidence Collector with `has_source_refutation`, separate Evidence Judge with `read_contains_line` gate, `needs_verify: false` merge before Dedup/Enrichment, removal of `SpecialistKind::Rules` with repository-instruction checks moved to the `Bug` specialist in Specialists mode, `fast` @ `High` for shard/collector/judge, context-line violations moved to the nearest `+` line of their hunk, diff-only fallback retaining `<rules>` in Generalist).
- Add Section 10 archive entry for `2026-11-25-review-engine-overhaul-design.md` and change its header to `Status: implemented`.

- [ ] **Step 2: Commit**

```sh
git diff --cached --name-only   # must not list .cargo/config.toml
git add docs/SPEC.md docs/superpowers/specs/2026-11-25-review-engine-overhaul-design.md
git commit -m "docs(spec): sync SPEC.md for Sharded Rules and mark overhaul spec implemented" -- docs/SPEC.md docs/superpowers/specs/2026-11-25-review-engine-overhaul-design.md
```
