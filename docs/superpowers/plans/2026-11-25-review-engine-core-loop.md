# Review Engine Core Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Overhaul Adus's core review loop so the Finder investigates all changed hunks thoroughly, rescues missed defects in Pass 2, verifies on `primary` at `High` effort with an Evidence Gate, and enriches surviving findings post-verify.

**Architecture:** `OpenAiClient` wires OpenRouter `reasoning` + `require_parameters` with fallback; `CoverageLedger` tracks per-hunk `read_file` intervals to drive per-step debt notes and a one-shot completion gate inside `run_react_loop`; Finder switches to raw diffs, 500-char PR descriptions, lean 5-field output, prose recovery, and Pass 2 `synthesis-rescue`; `verifier.rs` moves to `primary` @ `High` with no candidate count cap (7-step light + 10-step Evidence Gate, auto-widened `read_file`, no-tools final step); and `severity.rs` + `enrich.rs` classify severity, write titles/patches, and strip broken suggestions before posting.

**Tech Stack:** Rust (tokio, serde_json, reqwest), `crates/llm`, `crates/review`, `crates/adus` (`worker.rs` timeout assertions), `adus_git::diff::commentable_hunks`, sqlx Postgres + Redis (`crates/review/tests/engine.rs`).

**Spec:** [docs/superpowers/specs/2026-11-25-review-engine-overhaul-design.md](../specs/2026-11-25-review-engine-overhaul-design.md)

## Global Constraints

- **Clean-room and no-assumption rule (binding, verbatim from `AGENTS.md`):**
  1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. **Order of authority:** the task brief (or active unmerged change spec), then [docs/SPEC.md](docs/SPEC.md) (and [DESIGN.md](docs/DESIGN.md) for `web/`), then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. **Openness:** every implementer report lists its assumptions under an `"Assumptions"` heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. **Reviewers** flag as **Important** any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing [docs/SPEC.md](docs/SPEC.md) updates when SDD completes.
  5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
- **Model & Effort Matrix (Option 1):** `primary` @ `High` (Finder Pass 1 & Pass 2); `primary` @ `High` (Verifier light & Evidence Gate — the Finder's own effort, user decision 2026-11-26); `fast` @ `Medium` (Prose Recovery, Semantic Dedup, Severity Classifier, Post-Verify Enricher).
- **Exact Constants:**
  - `REVIEW_TIMEOUT = Duration::from_secs(20 * 60)` (`SPECIALISTS_REVIEW_TIMEOUT = 30 min`, `JOB_TIMEOUT = 31 min`)
  - `CACHE_READ_DIVISOR = 4`
  - `MAX_DESCRIPTION_CHARS_TOOLS = 500`, `MAX_DESCRIPTION_CHARS_DIFF_ONLY = 4_000`
  - `FINDER_BASE_STEPS = 20`, `FINDER_FREE_FILES = 8`, `FINDER_STEP_CAP = 30`, `FINDER_WRAP_UP_STEPS = 2`, `SYNTHESIS_MAX_STEPS = 12`
  - `GATE_MIN_STEPS_LEFT = 2`, `GATE_DOC_EXTENSIONS = ["md", "mdx", "rst", "txt"]`
  - `COVERAGE_DEBT_MAX_FILES = 8`, `COVERAGE_DEBT_MAX_RANGES = 3`
  - `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000`, `COMPACT_EXCERPT_CHARS = 400`, `COMPACTED_MARKER = "\n[older tool output shortened to save context]"`
  - `VERIFY_EFFORT = ReasoningEffort::High`, `VERIFY_LIGHT_MAX_STEPS = 7`, `VERIFY_FULL_MAX_STEPS = 10`, `VERIFY_WIDEN_SPAN_THRESHOLD = 20`, `VERIFY_WIDEN_BEFORE = 20`, `VERIFY_WIDEN_AFTER = 10`
  - `ENRICH_CONCURRENCY = 4`, `ENRICH_MAX_TOKENS = 2_048`, `ENRICH_RESERVE = Duration::from_secs(20)`, `SEVERITY_MAX_TOKENS = 1_024`, `DEDUP_MAX_TOKENS = 1_024`
  - `SYNTHESIS_MIN_BUDGET_SHARE = 25` (percent of `max_review_input_tokens`, weighted), `SYNTHESIS_MIN_WINDOW = max(6 × pass1_mean_step, 60 s)` (`pass1_mean_step` = 90 s when Pass 1 measured none)
  - `CHANGED_FUNCTIONS_MAX = 30`
  - Stage deadlines as fractions of the run's budget: `finder` 55 %, `verifier` 80 %, `post_verify` 93 % (renamed from `dedup`)
- **No dead code or stale tests (user requirement, 2026-11-26):** every task deletes, in the same commit, whatever its change makes obsolete: functions, constants, struct fields, enum variants, parameters, `#[allow(dead_code)]` escapes, prompt text, test helpers, fakes, fixtures, and tests that pin removed behaviour (rewrite a test that still covers a live behaviour instead of deleting it). No `#[allow(dead_code)]` or `_`-prefixed rename may be added to silence a warning. Doc comments that describe removed behaviour are fixed in the same commit. The task report lists what was deleted.
- **Standing Rules:** Zero `.unwrap()`/`.expect()` outside tests; no new `dyn Trait`; `spawn_blocking` for filesystem/regex work; do not touch `SpecialistKind::Rules` yet (that is handled in the Sharded Rules plan).
- **Never commit `.cargo/config.toml`** (local edit holding secrets). It was unstaged on 2026-11-26 (the Precondition re-checks it), and before every commit run `git diff --cached --name-only` and confirm it is absent. Every `git commit` in this plan commits only the listed paths (`git commit -m "<msg>" -- <paths>`), so nothing else in the index rides along.
- **Spec decisions this plan must not undo** (spec §3.1, §4.2, §6.3): Claude adaptive thinking keeps `"display": "summarized"`; the hidden `adus rereview` command stays as a documented debug-only command; the coverage debt note is append-only (kept in the conversation); enrichment never rewrites a verified `body`.

## Precondition

- [ ] **Step 0: Clean the index and the working tree baseline**

```powershell
git restore --staged .cargo/config.toml
git diff --cached --name-only
```
Expected: the second command prints nothing. Do not pop, drop or stage any stash. The uncommitted debug and experiment edits in `crates/adus/Cargo.toml`, `crates/adus/src/main.rs`, `crates/llm/src/anthropic.rs`, `crates/llm/src/types.rs`, `crates/review/src/{agent,engine,finder,prompt,verifier}.rs` are handled by Task 1.
- **Verification Commands:** `cargo test -p adus-llm -j 2`, `cargo test -p adus-review -j 2`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all` (dev Postgres/Redis running via `docker compose -f docker-compose.dev.yml -p adus-dev up -d`).
- **Commits:** Conventional Commits; never push.

## Review Focus

1. **OpenRouter non-reasoning fallback:** When an OpenRouter endpoint returns 404/400 `"No endpoints found that can handle the requested parameters"`, `OpenAiClient` drops `provider.require_parameters` (and falls back cleanly via `chat_with_effort_fallback`) without failing the review. Pinned in Task 1 (`openrouter_sends_reasoning_and_falls_back_when_no_endpoints`).
2. **Multi-hunk partial read coverage:** Reading lines 1–20 of a file whose second hunk is at lines 100–110 leaves the file pending with `(pending lines 100-110)` until a second `read_file` covers lines 100–110; truncated `read_file` output only marks the `[shown_start..=shown_end]` header range as read. Pinned in Task 3 (`coverage_ledger_tracks_per_hunk_unions_and_ignores_truncated_unread_lines`).
3. **Final-step JSON guarantee:** When `no_tools_on_last_step == true`, the last step sends `tools: []` and accepts a valid JSON answer via `parse_prose` without triggering a nudge loop or `Unparseable`. Pinned in Task 4 (`no_tools_on_last_step_strips_tools_and_accepts_prose`).
4. **Same-turn / failed reads + prose refusal:** `read_file` + `submit_findings` in the same turn, failed `read_file` (`BadRange`), and early prose-JSON submissions are still refused once with `is_error: false` and fall back to the held submission (`cut_short: true, gate_refused: true`) if the run later ends unparseable. Pinned in Task 4 (`gate_refuses_early_submit_and_recovers_held_on_unparseable`).
5. **Broken-patch downgrade without losing finding:** If post-verify `check_fix` detects unbalanced brackets, fence markers, identical code, or an out-of-hunk `start_line..=line`, it strips `suggestion` and `start_line` and keeps the finding as a plain comment. Pinned in Task 8 (`check_fix_strips_broken_patches_and_keeps_finding`).

---

### Task 1: Clean Live Tracing & OpenRouter Reasoning Parameters

**Files:**
- Modify: `crates/llm/src/anthropic.rs:127-134` (keep `"display": "summarized"`, replace the `TEMP(experiment)` comment)
- Modify: `crates/llm/src/openai.rs:22-30,126-175`
- Modify: `crates/llm/src/types.rs` (commit existing `ChatResponse::reasoning` + `collect_reasoning` + test)
- Modify: `crates/review/src/agent.rs`, `crates/review/src/finder.rs`, `crates/review/src/verifier.rs` (commit existing `adus_review::trace` spans & reasoning trace)
- Modify: `crates/review/src/engine.rs:117-123`, `crates/review/src/prompt.rs` (revert the `TEMP(experiment)` effort and Workflow wording; Tasks 2 and 5 set the real values)
- Modify: `crates/adus/src/main.rs`, `crates/adus/Cargo.toml` (keep the hidden `rereview` command and its `uuid` dependency; replace `TEMP(debug)` comments with doc comments)

**Interfaces:**
- Produces:
  - `OpenAiClient::is_openrouter(&self) -> bool` (true when parsed `base_url` host is `openrouter.ai` or ends with `.openrouter.ai`, case-insensitive).
  - OpenRouter `chat_once` request JSON sets `"reasoning": { "effort": e.as_str() }` and (when `require_parameters` is true) `"provider": { "require_parameters": true }`; retries once without `require_parameters` when `LlmError::Invalid(msg)` contains `"no endpoints found"` (case-insensitive) before `chat_with_effort_fallback`.

- [ ] **Step 1: Write failing OpenRouter wire tests in `crates/llm/src/openai.rs` (`mod tests`)**

Add unit tests using `wiremock` (already a `dev-dependency` of `adus-llm`):
- `openrouter_sends_reasoning_and_falls_back_when_no_endpoints`:
  1. Mock server whose URL host is checked via `OpenAiClient::with_openrouter_for_test` or `is_openrouter_host("https://openrouter.ai/api/v1")` helper (test `is_openrouter_host` directly for `https://openrouter.ai/api/v1`, `https://OPENROUTER.AI/v1`, `https://api.openai.com/v1` = false, `https://notopenrouter.ai/v1` = false).
  2. When OpenRouter client sends `effort: Some(ReasoningEffort::High)`: first request body contains `body["reasoning"]["effort"] == "high"`, `body["provider"]["require_parameters"] == true`, and does NOT set `"reasoning_effort"`. When mock responds HTTP 404/400 with `"No endpoints found that can handle the requested parameters"`, client retries once without `body["provider"]["require_parameters"]` and succeeds; non-OpenRouter client still sends `"reasoning_effort"`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-llm -j 2 -- openrouter_`
Expected: FAIL (`is_openrouter_host` / OpenRouter `reasoning` wiring not defined).

- [ ] **Step 3: Clean `TEMP(experiment)` in `anthropic.rs` and implement OpenRouter reasoning wiring in `crates/llm/src/openai.rs`**

- In `crates/llm/src/anthropic.rs:129-130`, keep `body["thinking"] = json!({"type": "adaptive", "display": "summarized"});` and replace the `TEMP(experiment)` comment with: `// "summarized": Claude 4.7+ / 5.x return empty thinking blocks by default; this changes only what the response shows, not cost or behaviour, and lets the trace show reasoning.` Extend the existing Anthropic request-body test to assert `thinking.display == "summarized"` for an adaptive-thinking model.
- In `crates/adus/src/main.rs`, keep `Command::Rereview` (still `#[command(hide = true)]`) and `rereview()`. Replace the `TEMP(debug)` comments with doc comments: `/// Debug only (hidden): queue a full review of a PR as if it had just been opened; a running \`adus serve\` picks it up.` In `crates/adus/Cargo.toml`, replace `# TEMP(debug): only for the \`rereview\` command.` with `# Used by the hidden \`rereview\` debug command.`
- In `crates/review/src/engine.rs:117-123`, revert the `TEMP(experiment)` Generalist effort to the committed value (Task 6 wires `High` with Pass 2). In `crates/review/src/prompt.rs`, revert the `TEMP(experiment)` Workflow and task-line wording to the committed text (`git diff crates/review/src/prompt.rs` must be empty afterwards); Task 5 rewrites these prompts.
- In `crates/llm/src/openai.rs`: add `fn is_openrouter_host(base_url: &str) -> bool` using `reqwest::Url::parse(base_url)` (`host_str()` exact `"openrouter.ai"` or `ends_with(".openrouter.ai")`, case-insensitive). Update `OpenAiClient::chat` and `chat_once(..., require_params: bool)` so OpenRouter sends `"reasoning": { "effort": e.as_str() }` + `"provider": { "require_parameters": true }`, retries once with `require_params = false` if the error is `LlmError::Invalid(m)` containing `"no endpoints found"`, and then delegates to `chat_with_effort_fallback`.
- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-llm -j 2 && cargo test -p adus-review -j 2 --lib && cargo build -p adus -j 2 && git grep -n "TEMP(" -- crates`
Expected: tests PASS, the binary builds, and `git grep` finds no `TEMP(` marker (exit code 1). `git diff --name-only crates/review/src/engine.rs crates/review/src/prompt.rs` prints nothing.

- [ ] **Step 5: Commit live tracing, OpenRouter wiring and the debug command**

```sh
git diff --cached --name-only   # must not list .cargo/config.toml
git add crates/llm/src/anthropic.rs crates/llm/src/openai.rs crates/llm/src/types.rs crates/review/src/agent.rs crates/review/src/finder.rs crates/review/src/verifier.rs crates/adus/src/main.rs crates/adus/Cargo.toml
git commit -m "feat(llm): wire OpenRouter reasoning parameters, live agent tracing and the rereview debug command" -- crates/llm/src/anthropic.rs crates/llm/src/openai.rs crates/llm/src/types.rs crates/review/src/agent.rs crates/review/src/finder.rs crates/review/src/verifier.rs crates/adus/src/main.rs crates/adus/Cargo.toml
```

---

### Task 2: Cache-Weighted Token Budget, 20-Min Review Timeout, Stage Deadlines & Adaptive Finder Step Cap

**Files:**
- Modify: `crates/review/src/budget.rs`
- Modify: `crates/review/src/agent.rs:153-169,239-241`
- Modify: `crates/review/src/engine.rs:50-53,117-123`
- Modify: `crates/review/src/finder.rs:38-41,232`
- Modify: `crates/review/src/dedup.rs:78`
- Modify: `crates/adus/src/worker.rs:17-27` (doc comment only)
- Modify: `crates/review/tests/engine.rs` (check `REVIEW_TIMEOUT` / `finder_max_steps` assertions)

**Interfaces:**
- Produces:
  - `pub(crate) const CACHE_READ_DIVISOR: u32 = 4;` and `TokenBudget::add(&self, input_tokens: u32, cache_read_tokens: u32)` in `crates/review/src/budget.rs`.
  - `pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(20 * 60);` in `crates/review/src/engine.rs`.
  - `pub(crate) fn finder_max_steps(files: usize) -> usize` in `crates/review/src/finder.rs`: `(20 + files.saturating_sub(8).div_ceil(2)).min(30)`.
  - `const FINDER_BASE_STEPS: usize = 20; const FINDER_FREE_FILES: usize = 8; const FINDER_STEP_CAP: usize = 30;`
  - `Deadlines { finder, verifier, post_verify }` in `crates/review/src/engine.rs` at 55 % / 80 % / 93 % of the run's budget (`dedup` field renamed `post_verify`; every reader of `deadlines.dedup` moves to `deadlines.post_verify`). Applies to both `REVIEW_TIMEOUT` and `SPECIALISTS_REVIEW_TIMEOUT`.
  - `TokenBudget::remaining_share_percent(&self) -> u32` (weighted budget left as a percentage of the limit; `100` when the budget is unlimited), consumed by Task 6's Pass 2 gate.

- [ ] **Step 1: Write failing tests in `budget.rs` and `finder.rs`**

- In `crates/review/src/budget.rs` tests: add `token_budget_discounts_cache_reads_by_quarter` asserting `budget.add(1_000, 0) -> 1_000`, `budget.add(1_000, 800) -> 1_000 + 200 + 200 (400)`, `budget.add(100, 500)` clamps `cached` to `100` (`25`), and odd `cached = 5` rounds up (`div_ceil(4) == 2`).
- In `crates/review/src/finder.rs` tests: add `finder_max_steps_scales_with_batch_files` asserting `0..=8 -> 20`, `9 -> 21`, `10 -> 21`, `11 -> 22`, `28 -> 30`, `100 -> 30`.
- In `crates/review/src/engine.rs` tests: assert `REVIEW_TIMEOUT == Duration::from_secs(20 * 60)`, and extend the existing `Deadlines` test (or add `deadlines_split_budget_55_80_93` if none exists) so a 20-minute budget gives `finder = start + 11 min`, `verifier = start + 16 min`, `post_verify = start + 18 min 36 s`.
- In `crates/review/src/budget.rs` tests: `remaining_share_percent` is `100` unlimited, `75` after a weighted quarter is spent, `0` when exhausted.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- token_budget_discounts finder_max_steps`
Expected: FAIL (`add` arity / `finder_max_steps` not defined).

- [ ] **Step 3: Implement changes in `budget.rs`, `agent.rs`, `dedup.rs`, `finder.rs`, `engine.rs`, and `worker.rs`**

- Update `TokenBudget::add(&self, input_tokens: u32, cache_read_tokens: u32)` using `CACHE_READ_DIVISOR = 4`: `cached = cache_read_tokens.min(input_tokens); uncached = input_tokens - cached; charged = uncached.saturating_add(cached.div_ceil(CACHE_READ_DIVISOR));`.
- Update all callers (`agent.rs` `chat_low_effort` and `run_react_loop`) to pass `response.cache_read_tokens`.
- Replace `FINDER_MAX_STEPS` in `finder.rs` with `finder_max_steps(batch.files.len())` passed into `run_one`.
- Update `REVIEW_TIMEOUT` to `Duration::from_secs(20 * 60)` in `engine.rs` and update the doc comment in `crates/adus/src/worker.rs:17` (`12 min` → `20 min`).
- Change `Deadlines::new` to `finder: at(55), verifier: at(80), post_verify: at(93)`, rename the field and its doc comment (`Finder (clone included) 55 %, Verifier 80 %, post-verify funnel 93 %; the rest is for posting`), and update every `deadlines.dedup` reader.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib && cargo test -p adus -j 2 --no-run`
Expected: PASS (compiles and all lib tests pass).

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/budget.rs crates/review/src/agent.rs crates/review/src/dedup.rs crates/review/src/finder.rs crates/review/src/engine.rs crates/adus/src/worker.rs
git commit -m "feat(review): discount cached tokens, scale finder steps, retune stage deadlines, and raise generalist timeout to 20m" -- crates/review/src/budget.rs crates/review/src/agent.rs crates/review/src/dedup.rs crates/review/src/finder.rs crates/review/src/engine.rs crates/adus/src/worker.rs
```

---

### Task 3: Per-Hunk `CoverageLedger` & `Toolbox::read_file_range`

**Files:**
- Create: `crates/review/src/coverage.rs`
- Modify: `crates/review/src/lib.rs` (add `mod coverage;`)
- Modify: `crates/review/src/tools.rs` (`Toolbox::read_file_range`, expose helper to parse `[path: lines X-Y of Z]`)

**Interfaces:**
- Produces:
  - `Toolbox::read_file_range(&self, call: &ToolCall) -> Option<(String, u32, u32)>`: returns `Some((rel, start, end))` when `call.name == "read_file"`, args deserialize, `path` resolves via `RepoWorkspace::resolve_existing_file`, and `start..=end` is the requested window (`start = start_line.unwrap_or(1).max(1)`, `end = end_line.unwrap_or(start + READ_MAX_LINES - 1).min(start + READ_MAX_LINES - 1)` when valid).
  - `parse_read_header_range(output: &str) -> Option<(u32, u32)>`: parses `[...: lines X-Y of Z]` from a `read_file` tool result (`(0, 0)` means empty file -> covers all hunks).
  - `pub(crate) struct CoverageLedger` in `crates/review/src/coverage.rs`:
    - `pub(crate) fn from_files(files: &[&FileDiff]) -> Self` (extracts right-side hunks via `adus_git::diff::commentable_hunks`)
    - `pub(crate) fn record_read(&mut self, rel_path: &str, start: u32, end: u32)` (merges overlapping/adjacent intervals where `cur.start <= prev.end + 1`, marks target covered when every declared hunk `[h_start, h_end]` is contained inside one merged `[r_start, r_end]` interval, or on any read when hunks is empty)
    - `pub(crate) fn pending_qualifying_files(&self) -> Vec<String>` (excludes `GATE_DOC_EXTENSIONS = ["md", "mdx", "rst", "txt"]` case-insensitively and `is_test_path(path)` matching `*_test.*`, `*.test.*`, `*.spec.*`, `tests/**`, `__tests__/**`)
    - `pub(crate) fn debt_note(&self, submit_name: &str) -> Option<String>` (exact text, written for Adus — it must not reproduce Kodus's debt-note wording: first line `Unread changed code. You have not opened these changed lines with read_file yet:`, then one line per file `- {path}: lines {a}-{b}, {c}-{d}` (a single line as `line {a}`, more than 3 ranges end with `, …`), then `Read them before you call {submit_name}. A grep or list_dir match does not count as reading them.`; formats up to `COVERAGE_DEBT_MAX_FILES = 8` pending **qualifying** files — the same set as `pending_qualifying_files()`, so docs and tests never appear — with up to `COVERAGE_DEBT_MAX_RANGES = 3` pending hunk ranges each; returns `None` when no qualifying file is pending)
    - `pub(crate) fn pending_signature(&self) -> Vec<(String, Vec<(u32, u32)>)>` (sorted qualifying files with their pending ranges; Task 4 sends a new debt note only when this differs from the signature of the last note sent)
    - `pub(crate) fn opened_files(&self) -> HashSet<String>` (every file with at least one successful `record_read`)

- [ ] **Step 1: Write failing tests in `coverage.rs` and `tools.rs`**

- In `tools.rs` tests: `read_file_range_resolves_dot_paths_and_header_ranges` verifying `./src/a.rs` resolves to `src/a.rs`, `grep` and `../src/a.rs` return `None`, and `parse_read_header_range("[src/a.rs: lines 10-45 of 100]\n...") == Some((10, 45))`.
- In `coverage.rs` tests: `coverage_ledger_tracks_per_hunk_unions_and_ignores_truncated_unread_lines` verifying:
  - A file with two hunks (`1..=10` and `50..=60`) stays pending after reading `1..=20`, its `debt_note` lists `(pending lines 50-60)`, and becomes covered once `45..=65` is read.
  - Adjacent reads (`1..=5` then `6..=10`) merge and cover `1..=10`.
  - Truncated read output (`[src/a.rs: lines 1-8 of 80]`) only records `1..=8`, NOT `1..=200`.
  - Docs (`README.MD`, `docs/guide.txt`) and test files (`internal/image/helpers_test.go`, `src/foo.test.ts`, `tests/e2e.rs`) are excluded from `pending_qualifying_files()`, `debt_note()` and `pending_signature()` while non-test code files remain required; a batch with only docs and tests pending gives `debt_note() == None`.
  - `pending_signature()` is unchanged by a read that covers no pending range and changes once a pending range is covered.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- coverage_ledger read_file_range`
Expected: FAIL (`coverage` module and `read_file_range` not defined).

- [ ] **Step 3: Implement `crates/review/src/coverage.rs` and `Toolbox::read_file_range` in `crates/review/src/tools.rs`**

Add `mod coverage;` to `crates/review/src/lib.rs`, implement `CoverageLedger` using `adus_git::diff::commentable_hunks` and `normalize_path`, and add `Toolbox::read_file_range` + `parse_read_header_range` in `tools.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib -- coverage_ledger read_file_range`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/coverage.rs crates/review/src/lib.rs crates/review/src/tools.rs
git commit -m "feat(review): add per-hunk CoverageLedger and read_file range tracking" -- crates/review/src/coverage.rs crates/review/src/lib.rs crates/review/src/tools.rs
```

---

### Task 4: ReAct Loop Policies — Debt Injection, One-Shot Gate, Budget Bands & Final-Step JSON

**Files:**
- Modify: `crates/review/src/agent.rs` (`AgentConfig`, `AgentOutcome`, `Tally`, `WrapUp`, `run_react_loop`, `run_tool_calls`, tests)
- Modify: `crates/review/src/finder.rs` (`run_one` sets `budget_bands: true` on every Finder `AgentConfig`, and sets `coverage_ledger: Some(CoverageLedger::from_files(..))` and `enforce_gate: true` only for `Generalist` and `Bug` with `AgentTools::Full`; `Security`, `Performance` and diff-only runs get `coverage_ledger: None`, `enforce_gate: false` — spec §4.2 "Which runs get a ledger")

**Interfaces:**
- Consumes: `CoverageLedger`, `Toolbox::read_file_range`, `parse_read_header_range` from Task 3.
- Produces:
  - `AgentConfig` fields (all defaulting in `AgentConfig::new`):
    - `pub coverage_ledger: Option<CoverageLedger>` (drives per-step debt notes when `with_repo_tools && !wrap_up.active()`)
    - `pub enforce_gate: bool` (enables the one-shot hard refusal for `Generalist` and `Bug` when qualifying files remain pending and `remaining_investigating_steps >= GATE_MIN_STEPS_LEFT (2)`)
    - `pub no_tools_on_last_step: bool` (strips ALL tools on the final step `steps_used + 1 == max_steps` and instructs the model to reply with final JSON). When set, the loop also appends once, before step `max_steps - 1`, the verdict-due note `One step left for reading. After it you must answer with your final JSON verdict, without tools, based on what you have read.` (append-only).
    - `pub budget_bands: bool` (default `false`; only Finder passes set it — the Verifier and rule-evidence runs must never receive the Finder's Encourage/Urgent texts)
    - `pub widen_short_reads: bool` (widens `<20`-line `read_file` calls by `-20/+10` lines when passed to `Toolbox`)
  - `AgentOutcome<T>` gains `pub gate_refused: bool` and `pub opened_files: HashSet<String>`.
  - `pub(crate) const GATE_MIN_STEPS_LEFT: usize = 2;`
  - Escalating `BudgetBand::{Free, Encourage, Urgent}` transition notes in `agent.rs` (replacing the old `steps_used == max_steps / 2` message), with these exact texts (`{submit}` is the run's submit tool name):
    - `Encourage`: `Step budget: {used} of {max} steps used. Keep going through the changed code you have not checked yet, and batch independent lookups in one turn.`
    - `Urgent`: `Step budget: {left} steps left before only {submit} is offered. Finish the check you are on, then call {submit} with every finding and dismissal so far.`
  - **Append-only debt note:** before a step, when `with_repo_tools && !wrap_up.active()`, `coverage_ledger.debt_note(submit_name)` is `Some`, and `coverage_ledger.pending_signature()` differs from the signature recorded when the last note was sent (none sent yet counts as different), push the note into `messages` (it stays in the conversation for every later request) and record the signature. Never send a message on one request that is missing from the next request's `messages`: the history stays append-only, which keeps the cache prefix valid and the Claude preserved-thinking history check satisfied (spec §4.2). The provider wire mapping keeps roles alternating; on Anthropic the note becomes a trailing text block of the user message that carries the step's `tool_result` blocks, because `wire_messages` in `crates/llm/src/anthropic.rs:276-282` already merges consecutive same-role messages.
  - **One-shot gate:** both tool-call and prose submissions trigger the one-shot refusal (`is_error: false` tool result, or a user message for prose) when the four spec §4.2 conditions hold. The first submission is held; a second `submit_findings` in the same turn is also refused; the held submission is returned with `cut_short: true, gate_refused: true` if the loop later exits unparseable. `submit_findings` is never withheld from the offered tools by the gate. **Empty resubmission:** when the accepted submission has no findings and an earlier submission of the run (held or refused) had findings, the outcome's findings are the most recent earlier non-empty ones (the accepted submission's `reasoning` and `dismissed` are kept).
  - `AgentOutcome` gains `pub mean_step: Option<Duration>` (mean wall time of the run's LLM steps), consumed by Task 6's Pass 2 time gate.

- [ ] **Step 1: Write failing unit tests in `crates/review/src/agent.rs`**

Add focused unit tests covering all gate, debt, band, widen, and last-step behaviors:
1. `coverage_debt_note_is_append_only_and_sent_on_change`: with a two-hunk file, verifies (a) the first request after the initial prompt carries a debt note listing both pending ranges; (b) a step whose reads cover nothing pending sends no new note; (c) after a read covers one hunk, the next request carries a new note listing only the remaining range; (d) every request's `messages` starts with the previous request's `messages` unchanged (append-only), so earlier notes are still present; (e) no note once nothing qualifying is pending or during wrap-up; (f) a batch whose only pending files are docs/tests gets no note.
2. `gate_refuses_early_submit_and_recovers_held_on_unparseable`: covers (a) same-turn `read_file` + `submit_findings` still refused once with `is_error: false`, (b) failed `read_file` (`BadRange`) does not satisfy gate, (c) two `submit_findings` in one turn are both refused, (d) early prose JSON is refused once with exact user message, and (e) subsequent unparseable prose returns the held submission with `cut_short: true, gate_refused: true`.
3. `gate_skipped_when_insufficient_steps_or_no_tools`: skipped when `max_steps - wrap_up_steps - 1 < 2` or `AgentTools::SubmitOnly`.
4. `budget_bands_emit_encourage_and_urgent_once`: with `max_steps = 12, wrap_up_steps = 2, budget_bands = true`, verifies `Encourage` fires once at step 3 (`10 - 7`) and `Urgent` once at step 7 (`10 - 3`), each with the exact text above, that neither text contains "as soon as" or "extra steps", and that the old `steps_used == max_steps / 2` note no longer fires. Also: no band note when `max_steps = 5`, when `budget_bands == false` (a 7-step Verifier-shaped config), or without repository tools.
5. `no_tools_on_last_step_strips_tools_and_accepts_prose`: with `max_steps = 3, no_tools_on_last_step = true`, step 3 request has `tools.is_empty() == true`, step 2's request ends with the verdict-due note (and step 3's still contains it), and valid JSON prose succeeds with `cut_short: false`.
6. `gate_empty_resubmit_keeps_earlier_findings`: the first submission (2 findings) is refused by the gate; the model reads the file and resubmits `{"reasoning": "...", "findings": []}`; the outcome has the 2 earlier findings and the second submission's `reasoning`.
7. `finder_ledger_only_for_generalist_and_bug` (in `finder.rs` tests): with repository tools, `Generalist` and `Bug` runs receive a debt note on their first step after the prompt, while `Security` and `Performance` runs on the same batch never receive a debt note or a gate refusal (an early `submit_findings` is accepted at once).
8. `widen_short_reads_expands_narrow_line_ranges`: verifies a `read_file` call with `start_line: 30, end_line: 35` executes against lines `10..=45`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- coverage_debt_note gate_refuses gate_empty_resubmit budget_bands no_tools_on_last_step widen_short_reads finder_ledger_only`
Expected: FAIL (`coverage_ledger`, `enforce_gate`, `no_tools_on_last_step`, `widen_short_reads`, `gate_refused`, `opened_files` not defined).

- [ ] **Step 3: Implement `AgentConfig`, `AgentOutcome`, `CoverageLedger` integration, `BudgetBand`, `no_tools_on_last_step`, and `widen_short_reads` in `crates/review/src/agent.rs`**

Update `agent.rs` (and adjust existing tests in `agent.rs` for `AgentOutcome`'s new fields and the new budget band copy). Record successful `read_file` completions into `coverage_ledger` and `opened_files` AFTER `run_tool_calls` returns (`!result.is_error`), parsing actual `[shown_start..=shown_end]` via `parse_read_header_range`. Fill `AgentOutcome::mean_step` from the measured LLM step durations so Pass 2 can check its time gate.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/agent.rs crates/review/src/finder.rs
git commit -m "feat(review): add append-only coverage debt notes, one-shot gate, Finder-only budget bands, and last-step text mode" -- crates/review/src/agent.rs crates/review/src/finder.rs
```

---

### Task 5: Remove Pre-Expanded Context, Cap PR Description & Upgrade to Adversarial Multi-Lens Prompts + Lean Finder Schema

**Files:**
- Modify: `crates/review/src/prompt.rs:19,61-65,135-278,325-420`
- Modify: `crates/review/src/tools.rs:421-467` (remove `load_expanded_diffs`)
- Modify: `crates/review/src/specialists.rs`
- Modify: `crates/review/src/parse.rs` (`Suggestion`, `parse_submit_findings`, `submit_findings_tool_def`)
- Modify: `crates/review/src/engine.rs:550-567` (stop calling `load_expanded_diffs`)

**Interfaces:**
- Consumes: `CoverageLedger` from Task 3.
- Produces:
  - `Suggestion` in `crates/review/src/parse.rs`:
    - `pub title: String` (empty during lean Finder output; filled by Enricher or fallback)
    - `pub severity: Option<Severity>` (`None` until `classify_severity` fills it, spec §6.2; `format_inline` and `engine.rs` read it with `unwrap_or(Severity::Warning)`).
    - `pub category: Option<FindingCategory>` (`Bug | Security | Performance | RepoInstruction`).
  - `format_pr_description(description: &str, max_chars: usize) -> String` with `MAX_DESCRIPTION_CHARS_TOOLS = 500` and `MAX_DESCRIPTION_CHARS_DIFF_ONLY = 4_000`.
  - `pub(crate) fn changed_functions(files: &[&FileDiff]) -> Vec<(String, Vec<String>)>` in `crates/review/src/prompt.rs`: per file, the enclosing-scope text after each hunk header (`@@ -a,b +c,d @@ <context>`), trimmed to before the first `(` or `{` when present, deduplicated per file in hunk order, at most `CHANGED_FUNCTIONS_MAX = 30` entries in total. Rendered as a `<changed_functions>` section in the tool-enabled Finder user prompt (`- path: name, name`); omitted when empty and in diff-only prompts.
  - Lean `submit_findings_tool_def(is_generalist: bool)`: properties in this order `reasoning` (string), `findings` (array of `{ file, start_line?, line, category, body }`), optional `dismissed`; `required: ["reasoning", "findings"]`. `reasoning` describes, for each changed function or hunk, what was challenged, which callers or callees were read, and why each suspicion was reported or dismissed.
  - `parse_submit_findings` accepts a submission whose `reasoning` is missing or empty (never refused; logs `finder reasoning missing` at debug) and logs a present `reasoning` at debug (`finder reasoning`, with `specialist` and `batch`); it is never posted.

- [ ] **Step 1: Write failing tests in `prompt.rs`, `parse.rs`, and `specialists.rs`**

- `prompt.rs`:
  - `finder_user_prompt_caps_description_at_500_with_tools_and_4000_diff_only`: asserts `with_tools = true` truncates a 1,000-char PR description at 500 chars and uses raw unified diff without `:` expanded lines; `with_tools = false` allows up to 4,000 chars.
  - `finder_system_prompt_requires_first_tool_call_and_removes_early_exit`: asserts system prompt does NOT contain `"nobody counts your findings"`, `"Do not keep searching just because steps remain"`, `"as soon as"` or `"extra steps"`, and contains `"first action must be a tool call"`, the caller-grep instruction for `<changed_functions>`, the finish condition (submit once `reasoning` accounts for every changed function and lens), and all 3 lenses (Bug / Security / Performance) with the error-path checklist (swallowed errors, error treated as a miss, dropped fallback, `NULL` in `CONCAT`) for `Generalist`.
  - `changed_functions_come_from_hunk_headers`: two files with headers `@@ -1,3 +1,4 @@ func GetCache(ctx context.Context) {` (twice) and `@@ -5,2 +5,3 @@ impl Foo {`, plus one header without context: yields `[("a.go", ["func GetCache"]), ("b.rs", ["impl Foo"])]`; the cap of 30 holds; the section is absent from diff-only prompts and when no header has context.
- `parse.rs`:
  - `parses_lean_finder_findings_and_back_compat_full_findings`: asserts `parse_submit_findings` parses `{ "reasoning": "...", "findings": [{ "file": "src/a.rs", "line": 4, "category": "bug", "body": "why" }] }` into `Suggestion { title: "", severity: None, category: Some(Bug), suggestion: None, .. }`, accepts the same payload without `reasoning`, AND still accepts legacy full objects with `severity` and `title`. Extend `submit_findings_tool_def_uses_portable_schema` to assert `required == ["reasoning", "findings"]` and that `reasoning` is listed before `findings`.
  - `parses_stringified_findings_and_dismissed_arrays`: `{"reasoning": "r", "findings": "[{\"file\": \"src/a.rs\", \"line\": 4, \"category\": \"bug\", \"body\": \"why\"}]"}` parses to one finding, a stringified `dismissed` array is parsed the same way, and `"findings": "not json"` or a stringified object is rejected as today (Claude models send arrays as JSON strings; spec §4.4).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- finder_user_prompt_caps finder_system_prompt_requires changed_functions_come parses_lean_finder parses_stringified`
Expected: FAIL.

- [ ] **Step 3: Implement prompt, schema, and raw-diff changes across `prompt.rs`, `tools.rs`, `specialists.rs`, `parse.rs`, and `engine.rs`**

- Remove `expand_diff_block`, `push_expanded_range`, `EXPANDED_HUNK_CONTEXT`, `MAX_EXPAND_FILE_CHARS`, `MAX_EXPANDED_CONTEXT_LINES_PER_FILE` from `prompt.rs` and `load_expanded_diffs` from `tools.rs` + `engine.rs` (also remove `expanded: &HashMap<String, String>` parameter from `batch_targets` and `build_finder_user_prompt`).
- Rewrite `FINDER_MINDSET`, `FINDER_WORKFLOW_TOOLS`, `FINDER_SCOPE`, `GENERALIST_ROLE`, `BUG_ROLE`, `SECURITY_ROLE`, `PERFORMANCE_ROLE` fresh for Adus with adversarial stance, first-step tool requirement, "grep the callers of each `<changed_functions>` entry (and of each changed hunk without one) and read what they pass and expect", 3-lens checklists including the error-path items, the finish condition replacing "submit immediately", and zero early-exit cues.
- Add `changed_functions` and render `<changed_functions>` in `build_finder_user_prompt` for tool-enabled runs.
- In `parse_submit_findings` (and `parse_dismissed`), when `findings` / `dismissed` is a string, parse it with `serde_json::from_str` and use it only if it yields a JSON array.
- Update `Suggestion` (`severity: Option<Severity>`, `category: Option<FindingCategory>`, `title: String`, `body: String`, `suggestion: Option<String>`), `submit_findings_tool_def`, and `parse_submit_findings` / `parse_suggestions`. Keep `format_inline` defaulting `severity.unwrap_or(Severity::Warning)` if unset.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/prompt.rs crates/review/src/tools.rs crates/review/src/specialists.rs crates/review/src/parse.rs crates/review/src/engine.rs crates/review/src/finder.rs
git commit -m "feat(review): switch Finder to raw diffs, changed-function hints, adversarial multi-lens prompts, and lean output with required reasoning" -- crates/review/src/prompt.rs crates/review/src/tools.rs crates/review/src/specialists.rs crates/review/src/parse.rs crates/review/src/engine.rs crates/review/src/finder.rs
```

---

### Task 6: Prose Recovery & Pass 2 Synthesis Rescue (`finder.rs`)

**Files:**
- Modify: `crates/review/src/finder.rs`
- Modify: `crates/review/src/parse.rs` (add `parse_prose_recovery` if needed)
- Modify: `crates/review/src/engine.rs` (`CandidateFinding.investigated_by_finder`)

**Interfaces:**
- Consumes: `AgentOutcome::opened_files`, `CoverageLedger`, `TokenBudget` from Tasks 2–4; `Suggestion` from Task 5.
- Produces:
  - `FinderLlm.fast_model: &'a str` (for Prose Recovery on `fast` @ `Medium`).
  - `FinderTask` / `FinderRun` returning `AgentOutcome<Vec<Suggestion>>` where `result` merges Pass 1 + Prose Recovery + Pass 2 `synthesis-rescue` (deduped by `(normalized_file, line, normalized_body_prefix)`), and `opened_files` is the union of files read in Pass 1 + Pass 2.
  - `pub(crate) fn looks_like_findings(text: &str) -> bool` and `recover_prose_findings(...)` called when a pass returns empty `findings` and prose/reasoning matches (`len >= 80`, >= 2 signal groups).
  - `SYNTHESIS_MAX_STEPS = 12`: Pass 2 runs on `Generalist` and `Bug` when `with_tools` and both gates hold:
    1. budget: `token_budget.remaining_share_percent() >= SYNTHESIS_MIN_BUDGET_SHARE (25)` (Task 2);
    2. time: `deadline.saturating_duration_since(Instant::now()) >= max(6 × pass1.mean_step.unwrap_or(90 s), 60 s)` (Task 4's `AgentOutcome::mean_step`).
    Otherwise Pass 2 is skipped with `SynthesisSkipped::{Budget, Time}`.
  - `pub(crate) enum FinderPass { Pass1, SynthesisRescue, ProseRecovery }` and `CandidateFinding.origin: FinderPass` (set per pass before the merge; a recovered finding is `ProseRecovery`).
  - `FinderRun` reports `synthesis_skipped: Option<SynthesisSkipped>` and `synthesis_candidates: usize` for the stage log.
  - `finder_effort(mode)` returns `ReasoningEffort::High` for both `Generalist` and `Specialists` Finder passes (spec §2).

- [ ] **Step 1: Write failing tests in `crates/review/src/finder.rs`**

Add unit tests with `ScriptedChat`:
1. `finder_runs_synthesis_rescue_and_merges_missed_findings`: Pass 1 reads `src/a.rs` and submits 1 finding; Pass 2 receives `<already_inspected_files>`, `<recent_investigation>`, and `<current_findings>`, reads `src/b.rs`, and submits 1 new finding + 1 duplicate of Pass 1. Assert `outcome.result.len() == 2`, `outcome.opened_files` contains both `src/a.rs` and `src/b.rs`, and Pass 2 had `enforce_gate == false`.
   Also assert each merged finding's `origin` (`Pass1` for the first, `SynthesisRescue` for the new one) and `synthesis_candidates == 1`.
2. `finder_skips_synthesis_rescue_on_budget_time_or_specialist`: Security/Performance specialists never run Pass 2; a Generalist with 20 % weighted budget left skips with `SynthesisSkipped::Budget`; a Generalist whose deadline leaves less than `max(6 × mean_step, 60 s)` skips with `SynthesisSkipped::Time` (use a 2 s `mean_step` and 50 s left to pin the 60 s floor); with 30 % budget and enough time Pass 2 runs.
3. `prose_recovery_extracts_findings_when_pass_omits_structured_array`: Pass 1 submits empty findings with finding-like reasoning prose (`src/a.rs:2` + bug/should keywords); `recover_prose_findings` runs once on `fast_model` @ `Medium` and recovers the finding anchored to `src/a.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- synthesis_rescue prose_recovery`
Expected: FAIL.

- [ ] **Step 3: Implement `looks_like_findings`, `recover_prose_findings`, and Pass 2 `synthesis-rescue` in `finder.rs` + wire `investigated_by_finder` in `engine.rs`**

- Capture tool-call trace summaries (`tool(args_excerpt_150)`) in `AgentOutcome` (or a small `Vec<String>` of last 20 tool call summaries) so `finder.rs` can render `<recent_investigation>` cleanly without re-scanning provider messages.
- Ensure Pass 2 prompt uses exact `prompt` prefix + `\n\n<already_inspected_files>...\n<recent_investigation>...\n<current_findings>...` for prompt-cache reuse.
- Add `pub investigated_by_finder: bool` and `pub origin: FinderPass` to `CandidateFinding` in `verifier.rs` and populate them in `engine.rs::collect_candidates` from `outcome.opened_files` and the per-pass origin.
- Set `finder_effort` to `High` for both modes in `engine.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/finder.rs crates/review/src/agent.rs crates/review/src/verifier.rs crates/review/src/engine.rs
git commit -m "feat(review): add prose-findings recovery and Pass 2 synthesis rescue with budget and time gates" -- crates/review/src/finder.rs crates/review/src/agent.rs crates/review/src/verifier.rs crates/review/src/engine.rs
```

---

### Task 7: Surgical Verifier (`primary` @ `High`) + Evidence Gate

**Files:**
- Modify: `crates/review/src/verifier.rs`
- Modify: `crates/review/src/engine.rs:604-646` (pass `primary_model` into `VerifyContext`; delete the `truncated_by_cap` warning block)

**Interfaces:**
- Consumes: `AgentConfig::no_tools_on_last_step`, `AgentConfig::widen_short_reads` from Task 4; `CandidateFinding.investigated_by_finder` from Task 6.
- Produces:
  - `const VERIFY_LIGHT_MAX_STEPS: usize = 7;`
  - `const VERIFY_FULL_MAX_STEPS: usize = 10;`
  - `const VERIFY_EFFORT: ReasoningEffort = ReasoningEffort::High;`
  - `VerificationReport.evidence_gate_runs: usize`
  - `verify_candidates` runs 7-step light verify (`no_tools_on_last_step: true`, `widen_short_reads: true`), then runs a 10-step full re-verify on any kept finding where `!candidate.investigated_by_finder` (`evidence.toolbox.is_some()`), replacing the light verdict with the Evidence Gate verdict.
  - Removed: `MAX_VERIFY_CANDIDATES`, `prioritize`, `VerificationReport.truncated_by_cap` (spec §5 item 6).
  - `fn verify_order(candidates: Vec<CandidateFinding>) -> Vec<(usize, CandidateFinding)>` replaces `prioritize`: tags each candidate with its original index, then stable-sorts by category rank (`Security` 0, `Bug` 1, `Performance` 2, `RepoInstruction` 3; a `None` category takes it from `candidate.specialist`: `Security` → `Security`, `Performance` → `Performance`, `Bug` / `Generalist` → `Bug`), then origin rank (`Pass1` 0, `ProseRecovery` 1, `SynthesisRescue` 2 — an explicit rank, not the enum's declaration order). Nothing is cut. Both the light pass and the Evidence Gate pass queue through it.

- [ ] **Step 1: Write failing tests in `crates/review/src/verifier.rs`**

- Update existing `assert_fast_and_low` helper to `assert_primary_and_high` (and update `verify` test helper to pass `"primary-model"`).
- Add `evidence_gate_re_verifies_uninvestigated_findings_at_full_steps`: two candidates (`investigated_by_finder = true` and `false`), both kept in 7-step light pass; Evidence Gate re-verifies only the `false` one with `max_steps = 10` and drops it when the full pass returns `keep: false`.
- Assert `widen_short_reads` and `no_tools_on_last_step` are enabled and `budget_bands` is disabled on Verifier `AgentConfig`, and that every Verifier request uses `primary-model` at `ReasoningEffort::High`.
- Add `verifier_keeps_unreadable_verdict_and_drops_on_error_or_deadline`: (a) a final reply `I think this is fine` (no JSON, no tool call) keeps the finding and logs it as unreadable; (b) a final reply containing `{"keep": false, "rationale": "guard at a.rs:3"}` inside prose drops it (recovered verdict); (c) a `submit_verdict` call with `"keep": "false"` (string) drops it; (d) a provider error and (e) a deadline cut both drop it as `dropped_unverified`.
- Replace the existing cap test (the one asserting `report.truncated_by_cap == 5` and `chat.requests().len() == MAX_VERIFY_CANDIDATES`) with `verifies_every_candidate_without_a_cap`: same diff-only setup with 30 candidates; asserts 30 requests and 30 verdicts.
- Add `verify_order_ranks_category_then_origin_then_position`: input, in order, `[performance/Pass1, bug/SynthesisRescue, None-category from SpecialistKind::Security/Pass1, bug/Pass1, repo_instruction/Pass1, bug/ProseRecovery, bug/Pass1]`; asserts the returned original indices are `[2, 3, 6, 5, 1, 0, 4]`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- evidence_gate_re_verifies verifier_keeps_unreadable verifies_every_candidate_without_a_cap verify_order_ranks`
Expected: FAIL.

- [ ] **Step 3: Implement `VERIFY_EFFORT = High`, `VERIFY_LIGHT_MAX_STEPS = 7`, `VERIFY_FULL_MAX_STEPS = 10`, `no_tools_on_last_step`, `widen_short_reads`, verdict recovery (loose `submit_verdict` args, then a JSON object or bare `"keep": true|false` in the reply text) with keep-on-unreadable, Evidence Gate pass, `verify_order` in place of the cap, and `primary_model` wiring in `verifier.rs` & `engine.rs`**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/verifier.rs crates/review/src/engine.rs
git commit -m "feat(review): upgrade Verifier to primary/High with 7-step light, auto-widen, keep-on-unreadable verdicts, 10-step Evidence Gate, and an uncapped category-ordered queue" -- crates/review/src/verifier.rs crates/review/src/engine.rs
```

---

### Task 8: Post-Verify `classify_severity`, `enrich`, Deterministic `check_fix`, and Full Integration Tests

**Files:**
- Create: `crates/review/src/severity.rs`
- Create: `crates/review/src/enrich.rs`
- Modify: `crates/review/src/lib.rs` (add `mod severity; mod enrich;`)
- Modify: `crates/review/src/dedup.rs` (use `ReasoningEffort::Medium`, preserve `Severity` / `rule_id` / `category` fields)
- Modify: `crates/review/src/engine.rs` (wire Dedup → `classify_severity` → `enrich` → `check_fix`, update `review stages finished` log fields)
- Modify: `crates/review/tests/engine.rs` (update scripted stage matching & `ReasoningEffort` assertions, add end-to-end tests)

**Interfaces:**
- Produces:
  - `pub(crate) async fn classify_severity(chat: &dyn ChatService, provider_id: Uuid, fast_model: &str, usage: &UsageContext, budget: Option<&TokenBudget>, findings: &mut [Suggestion])` in `crates/review/src/severity.rs` (only classifies items with `severity.is_none()`; defaults to `Severity::Warning` on LLM/parse failure).
  - `pub(crate) async fn enrich_suggestions(chat: &dyn ChatService, provider_id: Uuid, fast_model: &str, usage: &UsageContext, budget: Option<&TokenBudget>, toolbox: Option<&Toolbox>, files: &[FileDiff], allowed: &HashMap<String, CommentableRange>, findings: &mut [Suggestion], deadline: Instant)` in `crates/review/src/enrich.rs` (populates `title` and optional `start_line` & `suggestion` from `{ "title", "start_line"?, "suggestion"? }`; **never changes `body`** — the Verifier checked it, spec §6.3; any `body` key in the reply is ignored; the prompt says the title restates the body's problem and adds nothing the body does not say; concurrency `ENRICH_CONCURRENCY = 4`; no new call starts once less than `ENRICH_RESERVE = 20 s` is left to `deadline` (the engine passes `deadlines.post_verify`); a finding not enriched keeps the fallback title from the body's first sentence, capped at 100 chars; runs `check_fix` deterministically).
  - `pub(crate) fn check_fix(existing_lines: Option<&str>, suggestion: &str, start_line: Option<u32>, line: u32, range: Option<&CommentableRange>) -> bool` in `crates/review/src/enrich.rs` (returns `false` when empty, whitespace-identical to `existing_lines`, contains `` ``` ``, has unbalanced `()[]{}` outside string/char literals or simple balanced-delimiter check, or `start_line` fails `spans_one_hunk`).

- [ ] **Step 1: Write failing unit tests in `severity.rs`, `enrich.rs`, and integration tests in `crates/review/tests/engine.rs`**

- `severity.rs`: `classify_severity_fills_missing_and_preserves_existing`: pre-set `Some(Severity::Critical)` stays `Critical`; `None` gets classified from JSON response (`"critical" | "warning" | "info"`), and falls back to `Warning` on error.
- `enrich.rs`:
  - `enrich_populates_title_and_patch_and_falls_back_on_error`: a reply that also carries a rewritten `"body"` leaves `body` byte-identical; an LLM error gives the first-sentence fallback title (≤ 100 chars) and `suggestion: None`; with 10 s left to the deadline no enrichment call is made and every finding gets the fallback title.
  - `check_fix_strips_broken_patches_and_keeps_finding` (tests empty, whitespace-identical, fence `` ``` ``, unbalanced `{`, and out-of-hunk `start_line` — verifies `suggestion` and `start_line` become `None` while the `Suggestion` remains intact).
- `crates/review/tests/engine.rs`: update `Stage` detection in `support` (`Severity`, `Enrich`, `Synthesis`), update `ReasoningEffort` expectations (`High` Finder and Verifier, `Medium` Dedup/Severity/Enrich), and verify end-to-end Generalist flow with Pass 2, Verifier Evidence Gate, Dedup, Severity, and Enrich; the posted comment body equals the Finder's `body` (after redaction), and the `review stages finished` log carries the new fields below.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- classify_severity check_fix enrich_populates`
Expected: FAIL.

- [ ] **Step 3: Implement `severity.rs`, `enrich.rs`, update `dedup.rs` + `engine.rs` + `verifier.rs` + `crates/review/tests/engine.rs`**

- Run Semantic Dedup, `classify_severity` and `enrich_suggestions` all under `deadlines.post_verify` (Task 2).
- Add `origin = ?candidate.origin` to the Verifier's per-candidate verdict debug line in `verifier.rs`.
- Extend the `review stages finished` info log with `finder_steps`, `gate_refusals`, `synthesis_candidates`, `synthesis_skipped`, `kept_by_origin` (counts of posted findings per `FinderPass`), `evidence_gate_runs`, `verify_candidates` (queued), `dropped_unverified`, `budget_used` (weighted), `input_tokens` (raw), `cache_read_tokens`, `cache_write_tokens`, `token_budget_reached`.

- [ ] **Step 4: Run full workspace tests, clippy, and fmt**

Run:
```sh
cargo test -p adus-llm -j 2
cargo test -p adus-review -j 2
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: All unit & integration tests pass with 0 clippy warnings and clean formatting.

- [ ] **Step 5: Commit**

```sh
git add crates/review/src/severity.rs crates/review/src/enrich.rs crates/review/src/lib.rs crates/review/src/dedup.rs crates/review/src/engine.rs crates/review/src/verifier.rs crates/review/tests/engine.rs
git commit -m "feat(review): add post-verify severity classifier, title and suggestion enricher, and check_fix gate" -- crates/review/src/severity.rs crates/review/src/enrich.rs crates/review/src/lib.rs crates/review/src/dedup.rs crates/review/src/engine.rs crates/review/src/verifier.rs crates/review/tests/engine.rs
```

---

### Task 9: Sync `docs/SPEC.md` for Core Loop Changes

**Files:**
- Modify: `docs/SPEC.md` (§3.1, §6.1, §7, §8.3, §8.4, §10)

- [ ] **Step 1: Update `docs/SPEC.md`**

Sync every implemented constant and behaviour, following spec §12:
- §3.1: the hidden, debug-only `adus rereview <repository_id> <pr_number>` command.
- §6.1: OpenRouter `reasoning` + `require_parameters` and the "no endpoints found" retry (superseding the OpenRouter part of the 2026-11-24 reasoning spec); Claude adaptive thinking with `display: "summarized"`.
- §7: `REVIEW_TIMEOUT = 20 min`; stage deadlines 55 / 80 / 93 % (`post_verify`), `ENRICH_RESERVE`.
- §8.3: removal of `expand_diff_block` / `load_expanded_diffs`; Verifier auto-widen.
- §8.4: model/effort matrix; raw diffs; 500-char PR description; `<changed_functions>`; per-hunk `CoverageLedger` with the append-only debt note and the one-shot gate (Generalist and Bug Pass 1 only); budget bands and their texts; required `reasoning`; Synthesis Rescue with its budget/time gates, `origin` and `synthesis_skipped`; Prose Recovery; `primary`/`High` Verifier with the verdict-due note, keep-on-unreadable verdicts and the Evidence Gate; no `MAX_VERIFY_CANDIDATES` cap and the `verify_order` queue (replace the §7 "Up to `MAX_VERIFY_CANDIDATES = 25` sorted `Critical > Warning > Info`" text); stringified-array parsing; the empty-resubmit fallback; Finder-only budget bands; post-verify Severity, title-and-suggestion Enrichment (body never rewritten) and `check_fix`; cache-weighted `TokenBudget`; the new stage-log fields.
- §10: an entry for the overhaul spec noting the core loop is implemented and Sharded Rules arrives in the companion plan (the spec stays `Status: draft` until that plan's last task).

- [ ] **Step 2: Commit**

```sh
git add docs/SPEC.md
git commit -m "docs(spec): sync SPEC.md with Core Loop overhaul" -- docs/SPEC.md
```

---

### Task 10: Live Acceptance Run on better-booru PR #13 (manual)

**Files:** none (results recorded in the PR description or the plan's notes; no code change unless a criterion fails).

This is spec §9.2. It needs a user's running stack and LLM keys, so it is run by the user (or with them), not by a subagent; the companion Sharded Rules plan starts only after it passes or its failures are triaged.

- [ ] **Step 1: Run the review with DeepSeek through OpenRouter as `primary`**

```powershell
$env:NO_COLOR = "1"; $env:RUST_LOG = "info,adus_review=debug,adus_review::trace=trace"; cargo run -p adus *> .superpowers/run-deepseek.log
```
In a second terminal: `cargo run -q -p adus -- rereview <repository_id> 13`. Wait for `job finished` on the `review_pr` job.

- [ ] **Step 2: Repeat with Claude Sonnet 5.5 as `primary`** (switch the repository's provider model in the dashboard; log to `.superpowers/run-sonnet.log`).

- [ ] **Step 2b: Repeat in `Specialists` mode with DeepSeek as `primary`** (switch the repository's review mode to Specialists in its settings; log to `.superpowers/run-specialists.log`). Switch the mode back afterwards.

- [ ] **Step 3: Check the pass criteria against the logs and the posted comments**

1. DeepSeek: at least 3 of the 4 reference defects are posted (`internal/api/posts.go:165` orphaned rows reaching `PreviewDimExpr`; `internal/image/cache.go` swallowed `_, _ =` update errors; the `s3Storage.Exists` error treated as a miss; the CDN branch falling through to the hard-coded `"webp"` fallback), and the url_builder and migration findings are still raised as candidates.
2. Sonnet 5.5: Pass 1 opens every qualifying changed file with `read_file` before its accepted submission (trace `agent tool call` lines), and at least 3 of the 4 reference defects are posted.
3. DeepSeek, Specialists: at least 3 of the 4 reference defects are posted; the `Bug` specialist's Pass 1 opens every qualifying changed file before its accepted submission; `Security` and `Performance` traces show no debt note and no gate refusal; `job finished outcome="done"` inside `SPECIALISTS_REVIEW_TIMEOUT`.
4. All three: `job finished outcome="done"` inside the mode's timeout, `token_budget_reached=false`, no `synthesis_skipped` (for `Bug` in Specialists mode), no Finder run ending at its step cap after a gate refusal; record `budget_used`, `input_tokens`, wall time and `kept_by_origin` from `review stages finished`. If the Specialists run hits the token budget, record which stage was cut (Pass 2 skipped or Verifier candidates dropped).

- [ ] **Step 4: Record the outcome**

Write the three runs' results (criteria met or not, `budget_used`, `input_tokens`, wall time, `kept_by_origin`) in the PR description. If a criterion fails, name the stage the trace points to (coverage, prompt, rescue, Verifier) and stop before the Sharded Rules plan.
