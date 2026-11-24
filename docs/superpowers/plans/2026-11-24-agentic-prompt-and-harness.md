# Agentic Prompt Redesign & Review Harness Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate CTF step-burn loops, premature compaction re-reads, Verifier PR-intent blindness, specialist gaps, silent line drops, and 3-line diff blindness across `crates/review` via redesigned role prompts, targeted harness fixes, and bounded pre-seeded hunk context.

**Architecture:** All changes live inside `crates/review` (plus `docs/SPEC.md` and spec sync) with zero DB migrations, REST API changes, or UI changes. Static prompt ordering and cache prefixes stay intact while adding role-specific tool playbooks, preview-preserving compaction (`320_000` chars / `400`-char excerpt), one-shot `submit_findings` anchor bounce, Verifier PR-intent + `drop_suggestion`, and ±15-line workspace-backed hunk expansion.

**Tech Stack:** Rust (`tokio`, `serde_json`, `sqlx::test` for `crates/review/tests/engine.rs`), existing `ScriptedChat` / `RoutedChat` / `RepoWorkspace` test fakes.

**Spec:** [docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md](../specs/2026-11-24-agentic-prompt-and-harness-design.md)

## Global Constraints

- **Clean-room and no-assumption rule (binding, verbatim from `AGENTS.md`):**
  1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. **Order of authority:** the task brief (or active unmerged change spec), then [docs/SPEC.md](docs/SPEC.md) (and [DESIGN.md](docs/DESIGN.md) for `web/`), then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. **Openness:** every implementer report lists its assumptions under an `"Assumptions"` heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. **Reviewers** flag as **Important** any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing [docs/SPEC.md](docs/SPEC.md) updates when SDD completes.
  5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
- **Exact constants & contracts (from Spec):**
  - Compaction: `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000`, `COMPACT_EXCERPT_CHARS = 400`, `COMPACTED_MARKER = "\n[older tool output shortened to save context]"` (guard `len <= 446` and `!ends_with(COMPACTED_MARKER)`).
  - Expanded hunk context: `EXPANDED_HUNK_CONTEXT = 15`, `MAX_EXPAND_FILE_CHARS = 20_000`, `MAX_EXPANDED_CONTEXT_LINES_PER_FILE = 120`, line prefix `{n:>5} : {code}`, secret-redacted on `:` lines only while raw hunk lines stay unredacted.
  - Specialist opening sentences (`"You are the generalist reviewer"`, `"You are the correctness reviewer"`, `"You are the security reviewer"`, `"You are the performance reviewer"`, `"You are the team-rules reviewer"`) MUST remain verbatim so `crates/review/tests/engine.rs` role matchers continue to work.
  - `build_finder_user_prompt` continues ending with `"call submit_findings."` (tools) / `"respond with the JSON array."` (diff-only).
  - `VERIFIER_ROLE` continues stating `` `start_line`..`line` is the span `` and `VERIFIER_DROP_RULES` continues starting the Rules clause with ``Findings reported by the Rules reviewer (`reported_by`)``.
- **Rust & workspace rules:** Zero `.unwrap()` / `.expect()` outside tests; no new `dyn Trait`; `spawn_blocking` for filesystem work; no DB migrations, REST API, `web/`, or `crates/llm` changes.
- **Precondition (workspace state):** The user's current uncommitted local edits (`.cargo/config.toml`, `crates/adus/Cargo.toml`, `crates/adus/src/main.rs`, `crates/llm/src/types.rs`, `crates/review/src/agent.rs`, `crates/review/src/finder.rs`, `crates/review/src/verifier.rs`) and untracked draft `docs/superpowers/specs|plans/2026-11-24-finder-completion-gate-and-verifier-effort*` are NOT part of this plan; stash or commit them on a separate branch before starting Task 1, and never stage them into this plan's commits.
- **Verification commands:** `cargo test -p adus-review -j 2`, `cargo test --workspace -j 2`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`. Never push without user request.

## Review Focus

1. Secret 5 lines outside a hunk in a modified file is redacted to `[REDACTED]` in `:` lines while a `+`-line secret in the same file remains raw, and `:` lines are rejected as comment anchors (pinned in Task 5 + Task 3).
2. Partial-valid `submit_findings` (1 valid `+` line + 1 off-line) on the final step (`can_retry == false`) or after 1 anchor retry succeeds instead of failing `Unparseable`, keeps the valid finding, and records `filtered_dropped = 1` (pinned in Task 3).
3. Compaction leaves 401..=446-char tool results untouched and never double-appends `COMPACTED_MARKER` on subsequent turns (pinned in Task 2).
4. Verifier omitting `drop_suggestion` preserves `suggestion` + `start_line`, while `"drop_suggestion": true` clears both while keeping the finding (pinned in Task 4).
5. `ReviewMode::Specialists` with empty DB `rules` and `repo_instructions: Some(...)` spawns `SpecialistKind::Rules` (pinned in Task 1).

---

### Task 1: Role prompts, specialist playbooks, `Rules` activation, and Dedup tie-breaking

**Files:**
- Modify: `crates/review/src/prompt.rs:94-175`
- Modify: `crates/review/src/specialists.rs:18-128`
- Modify: `crates/review/src/finder.rs:142`
- Modify: `crates/review/src/dedup.rs:24-33`
- Test: `crates/review/src/prompt.rs:653-688`, `crates/review/src/specialists.rs:105-128`, `crates/review/tests/engine.rs:2031-2056`

**Interfaces:**
- Produces: `active_specialists(mode: ReviewMode, has_rules_or_instructions: bool) -> Vec<SpecialistKind>` in `crates/review/src/specialists.rs`; updated `FINDER_*`, `<Role>` blocks, and `DEDUP_SYSTEM`.

- [ ] **Step 1: Write the failing tests**

Update `crates/review/src/prompt.rs` (`specialists_share_byte_identical_system_prefix`) and `crates/review/src/specialists.rs` (`active_specialists_omits_rules_when_none_match`), and add an assertion block to `repo_instructions_loaded_from_clone_into_prompt` in `crates/review/tests/engine.rs` (or a sibling specialists assertion in that test):

```rust
// In crates/review/src/prompt.rs (inside specialists_share_byte_identical_system_prefix)
assert!(tools.contains("empty list") || tools.contains("[]"));
assert!(tools.contains("up to 4") || tools.contains("parallel"));
assert!(tools.contains("`:`") || tools.contains("lines marked \":\""));
assert!(!plain.contains("submit_findings"));

// In crates/review/src/specialists.rs (extend active_specialists_omits_rules_when_none_match)
assert!(SpecialistKind::Bug.role_block().contains("grep"));
assert!(SpecialistKind::Security.role_block().contains("validation"));
assert!(SpecialistKind::Performance.role_block().contains("scale"));
assert!(SpecialistKind::Rules.role_block().contains("<repository_instructions>"));
```

And in `crates/review/tests/engine.rs` (`repo_instructions_loaded_from_clone_into_prompt`), also run a `ReviewMode::Specialists` pass with zero DB rules on the same setup and assert `finder.len() == 4` and `finder.iter().filter(|c| c.system.contains(RULES_ROLE)).count() == 1`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- specialists_ repo_instructions_loaded`
Expected: FAIL (prompts lack parallel/early-exit/`:` rules; `Specialists` with no DB rules only spawns 3 agents).

- [ ] **Step 3: Implement prompt & specialist updates**

- In `crates/review/src/prompt.rs`: rewrite `FINDER_MINDSET`, `FINDER_WORKFLOW_TOOLS`, `FINDER_WORKFLOW_DIFF_ONLY`, `FINDER_SCOPE`, `FINDER_SEVERITY`, and `FINDER_FIELDS` per Spec §4.2 while keeping `<ReviewInstructions>` byte-identical across all 5 `SpecialistKind` values and keeping `FINDER_WORKFLOW_DIFF_ONLY` free of `read_file`/`grep`/`list_dir`/`find_files`/`submit_findings`.
- In `crates/review/src/specialists.rs`: expand `GENERALIST_ROLE`, `BUG_ROLE`, `SECURITY_ROLE`, `PERFORMANCE_ROLE`, and `RULES_ROLE` into tool-driven playbooks per Spec §4.3 (preserving each opening sentence verbatim), and rename `active_specialists(mode: ReviewMode, has_rules_or_instructions: bool)`.
- In `crates/review/src/finder.rs:142`: pass `!batch.rules.is_empty() || input.repo_instructions.is_some()` to `active_specialists`.
- In `crates/review/src/dedup.rs:24-33`: update `DEDUP_SYSTEM` Task 1 tie-breaking per Spec §4.4.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 -- specialists_ repo_instructions_loaded dedup`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/prompt.rs crates/review/src/specialists.rs crates/review/src/finder.rs crates/review/src/dedup.rs crates/review/tests/engine.rs
git commit -m "feat(review): upgrade role prompts, specialist playbooks, and repo-instructions Rules activation"
```

---

### Task 2: Preview-preserving compaction and neutral halfway pacing note

**Files:**
- Modify: `crates/review/src/agent.rs:31-45`, `:211-220`, `:582-611`, `:1185-1243`
- Test: `crates/review/src/agent.rs:1185-1243`

**Interfaces:**
- Produces: `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000`, `COMPACT_EXCERPT_CHARS = 400`, `COMPACTED_MARKER = "\n[older tool output shortened to save context]"` in `crates/review/src/agent.rs`.

- [ ] **Step 1: Write the failing tests**

Update `compacts_oldest_tool_outputs_in_run_when_exceeding_char_limit` and `compact_respects_limit_protects_tail_and_terminates` in `crates/review/src/agent.rs`:

```rust
// Expect first COMPACT_EXCERPT_CHARS (400) chars + COMPACTED_MARKER instead of OMITTED_TOOL_OUTPUT.
let expected_short = format!("{}{COMPACTED_MARKER}", &"x".repeat(COMPACT_EXCERPT_CHARS));
assert_eq!(tool_results(&msgs[2])[0].content, expected_short);

// Boundary & idempotence cases in compact_respects_limit_protects_tail_and_terminates:
// 1. 446-char tool output (between 401 and 446) stays untouched when limit = 0.
// 2. Output already ending with COMPACTED_MARKER is never truncated or re-marked again.
```

Also assert in an `agent.rs` step-budget test that at `max_steps / 2` the injected pacing note contains `"do not spend extra steps"` and `"parallel"`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- compact`
Expected: FAIL (`OMITTED_TOOL_OUTPUT` / `COMPACT_EXCERPT_CHARS` mismatch).

- [ ] **Step 3: Implement preview-preserving compaction & halfway pacing**

In `crates/review/src/agent.rs`:
- Set `DEFAULT_COMPACTION_CHAR_LIMIT: usize = 320_000`, `COMPACT_EXCERPT_CHARS: usize = 400`, `COMPACTED_MARKER: &str = "\n[older tool output shortened to save context]"`; remove `OMITTED_TOOL_OUTPUT` and `COMPACT_MIN_CHARS`.
- Update `compact(messages, limit)` per Spec §5.1: compute `guard = COMPACT_EXCERPT_CHARS + COMPACTED_MARKER.chars().count()` (`446`); skip outputs with `len <= guard` or `result.content.ends_with(COMPACTED_MARKER)`; otherwise truncate with `truncate_with_marker(&result.content, COMPACT_EXCERPT_CHARS, COMPACTED_MARKER)` and update `total`.
- Update the halfway message in `run_react_loop` per Spec §5.2.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 -- agent::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/agent.rs
git commit -m "feat(review): preserve 400-char excerpts in compaction and refine step pacing note"
```

---

### Task 3: `submit_findings` anchor validation and `filtered_dropped` observability

**Files:**
- Modify: `crates/review/src/agent.rs:115-129`, `:273-284`, `:713-736`
- Modify: `crates/review/src/parse.rs:220-251`, `tests`
- Modify: `crates/review/src/finder.rs:51-60`, `:120-163`, `:232-250`
- Modify: `crates/review/src/verifier.rs:425-437`
- Modify: `crates/review/src/engine.rs:557-596`, `:680-696`, `:875-962`
- Test: `crates/review/src/parse.rs`, `crates/review/src/finder.rs` (or `crates/review/tests/engine.rs`)

**Interfaces:**
- Consumes: `CommentableRange` (`crates/review/src/parse.rs`).
- Produces: `SubmitSpec::parse_arguments(&self, arguments: &Value, can_retry: bool) -> Result<Self::Output, String>`, `parse::validate_batch_anchors(&[Suggestion], &HashMap<String, &CommentableRange>) -> Result<(), String>`, `FinderInput::allowed: &'a HashMap<String, CommentableRange>`, and `filtered_dropped: usize` returned by `collect_candidates`.

- [ ] **Step 1: Write the failing tests**

- In `crates/review/src/parse.rs`, add `validate_batch_anchors_rejects_bad_file_or_line_and_suggests_nearest`:
  - Valid `+` lines `[10, 12, 20, 21, 22, 30]` on `"src/a.rs"`:
    - Finding on `"src/missing.rs"` returns `Err` mentioning `"src/missing.rs"` and `"src/a.rs"`.
    - Finding on `"src/a.rs"` line `14` (e.g. a `:` context line) returns `Err` mentioning `14` and nearest valid `+` lines (`10, 12, 20, 21, 22`).
    - Finding on `"./src/a.rs"` line `12` returns `Ok(())`.
- In `crates/review/src/finder.rs` (unit test with `ScriptedChat`):
  - `finder_bounces_off_line_anchor_once_then_accepts_partial_on_second_or_last_step`:
    1. When `steps_used < max_steps` and first `submit_findings` names an invalid line, `SubmitFindings` returns an error tool result listing nearest valid `+` lines; agent re-submits on the valid `+` line and succeeds.
    2. When the second `submit_findings` still includes one invalid line alongside one valid finding (or when `can_retry == false` on the final step), `SubmitFindings` accepts `Ok(vec![...])` without failing `Unparseable`, leaving `keep_valid_batch` to filter the bad entry.
- In `crates/review/tests/engine.rs`: assert a mixed valid + off-line final submission posts the valid finding without failing `Unparseable`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- validate_batch_anchors finder_bounces`
Expected: FAIL (`validate_batch_anchors` not defined).

- [ ] **Step 3: Implement anchor validation and `filtered_dropped` counter**

- `crates/review/src/agent.rs`: add `can_retry: bool` to `SubmitSpec::parse_arguments`, passing `tally.steps_used < config.max_steps` at the call site; update `SubmitAnswer` in tests.
- `crates/review/src/verifier.rs`: update `SubmitVerdict::parse_arguments(&self, arguments: &Value, _can_retry: bool)`.
- `crates/review/src/parse.rs`: implement `validate_batch_anchors(suggestions, allowed_by_file)` per Spec §6.2 (5 nearest `+` lines sorted by `abs_diff`, tie-break smaller line, rendered ascending).
- `crates/review/src/finder.rs`: add `pub allowed: &'a HashMap<String, CommentableRange>` to `FinderInput<'a>`; construct per-batch `SubmitFindings { def, allowed, anchor_retried: AtomicBool::new(false) }` in `run_all` and pass `&submit` to `run_one`; enforce single-bounce anchor check in `parse_arguments`.
- `crates/review/src/engine.rs`: pass `&allowed` in `FinderInput`, update `collect_candidates` to return `Result<(Vec<CandidateFinding>, usize), EngineError>` counting `filtered_dropped`, and log `filtered_dropped` in `review stages finished`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 -- validate_batch_anchors finder_bounces`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/agent.rs crates/review/src/parse.rs crates/review/src/finder.rs crates/review/src/verifier.rs crates/review/src/engine.rs crates/review/tests/engine.rs
git commit -m "feat(review): validate submit_findings anchors once and log filtered_dropped"
```

---

### Task 4: Verifier PR-intent prefix, verdict rules, and `drop_suggestion`

**Files:**
- Modify: `crates/review/src/prompt.rs:18`, `:191-194`
- Modify: `crates/review/src/verifier.rs:122-160`, `:204-250`, `:316-396`, `:407-570`, `tests`
- Modify: `crates/review/src/engine.rs:610-620`
- Test: `crates/review/src/verifier.rs:1010-1185`

**Interfaces:**
- Consumes: `format_pr_description(&str) -> String` (`crates/review/src/prompt.rs`).
- Produces: `VerifyInput<'a>` with `pub pr: &'a PullRequest` and `pub manifest: &'a str`; `Verdict { pub keep: bool, pub drop_suggestion: bool }`; `Outcome::Kept { drop_suggestion: bool }`.

- [ ] **Step 1: Write the failing tests**

Update `crates/review/src/verifier.rs` unit tests (`user_prompt_puts_shared_sections_first_and_the_candidate_last` and `diff_only_and_tools_unsupported_fallback`):
- Assert `<pr_title>`, `<pr_description>`, `<pr_manifest>` appear in `verifier::build_user_prompt` ahead of `<rules>`, `<memories>`, `<repository_instructions>`, `<diff>`, `<code_context>`, `<candidate>`, and neutralize closing tags.
- Assert `build_system_prompt` mentions `<memories>`, `<pr_description>`, `drop_suggestion`, and parallel tool calls (when `with_tools = true`).
- Add `drop_suggestion_strips_patch_while_omission_preserves_it`:
  - Candidate with `start_line: Some(1)`, `line: 2`, `suggestion: Some("bad()")` gets `{"keep": true, "drop_suggestion": true, "rationale": "bad patch"}` → kept candidate has `suggestion == None` and `start_line == None`.
  - Second candidate with `suggestion: Some("good()")` gets `{"keep": true, "rationale": "ok"}` (omitting `drop_suggestion`) → kept candidate preserves `start_line` and `suggestion`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- verifier::tests`
Expected: FAIL (missing `pr`/`manifest` fields and `drop_suggestion`).

- [ ] **Step 3: Implement Verifier PR context, verdict rules, and `drop_suggestion`**

- In `crates/review/src/prompt.rs`: extract `pub(crate) fn format_pr_description(description: &str) -> String` and use it in `build_finder_user_prompt`.
- In `crates/review/src/verifier.rs`:
  - Add `pr` and `manifest` to `VerifyInput<'a>` and `Evidence<'a>`, and render `<pr_title>`, `<pr_description>`, `<pr_manifest>` at the top of `build_user_prompt`.
  - Update `VERIFIER_DROP_RULES`, `VERIFIER_WORKFLOW_TOOLS`, `VERIFIER_UNTRUSTED_INPUT`, `VERIFIER_OUTPUT_TOOLS`, `VERIFIER_OUTPUT_DIFF_ONLY` per Spec §7.2–§7.3 (preserving `"Findings reported by the Rules reviewer (`reported_by`)"` and ``"`start_line`..`line` is the span"``).
  - Replace `SubmitSpec::Output = bool` with `Verdict { keep: bool, drop_suggestion: bool }` (`#[serde(default)] drop_suggestion: bool` on `SubmitVerdictArgs`, optional `"drop_suggestion"` boolean in `submit_verdict_tool_def`), `Outcome::Kept { drop_suggestion: bool }`, and strip `suggestion` + `start_line` in `summarize` when `drop_suggestion == true`.
  - Update test helper `verify(...)` in `verifier.rs` and the caller in `crates/review/src/engine.rs:610-620` to pass `pr` and `manifest`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2 -- verifier::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/prompt.rs crates/review/src/verifier.rs crates/review/src/engine.rs
git commit -m "feat(review): add PR intent context, memory/hedge verdict rules, and drop_suggestion to verifier"
```

---

### Task 5: Pre-seeded expanded hunk context (`±15` lines) in Finder

**Files:**
- Modify: `crates/review/src/prompt.rs:179-319`, `tests`
- Modify: `crates/review/src/tools.rs:398-418`, `tests`
- Modify: `crates/review/src/finder.rs:51-60`, `:133-141`
- Modify: `crates/review/src/engine.rs:510-565`
- Test: `crates/review/src/prompt.rs`, `crates/review/src/tools.rs`, `crates/review/tests/engine.rs:2537`

**Interfaces:**
- Consumes: `read_text` (`crates/review/src/tools.rs`), `redact_secrets` (`crates/review/src/redact.rs`), `hunk_right_start` (`adus_git::diff`).
- Produces: `EXPANDED_HUNK_CONTEXT = 15`, `MAX_EXPAND_FILE_CHARS = 20_000`, `MAX_EXPANDED_CONTEXT_LINES_PER_FILE = 120`, `pub(crate) fn expand_diff_block(file: &FileDiff, head_text: &str) -> String`, `batch_targets(..., expanded: &HashMap<String, String>)`, `build_finder_user_prompt(..., expanded: &HashMap<String, String>)`, `tools::load_expanded_diffs(ws: Arc<RepoWorkspace>, files: &[FileDiff]) -> HashMap<String, String>`, and `FinderInput::expanded: &'a HashMap<String, String>`.

- [ ] **Step 1: Write the failing tests**

- In `crates/review/src/prompt.rs`:
  - `expand_diff_block_widens_hunks_merges_overlaps_caps_and_redacts_only_outside_hunk`:
    1. A 60-line modified file with two nearby hunks expands up to `15` lines before/after each hunk using `{n:>5} : {code}` without duplicating hunk lines or overlapping lines between hunks.
    2. A secret (`ghp_...`) placed 5 lines outside a hunk is masked to `[REDACTED]` in the `:` line, while a secret on a `+` line inside the hunk stays raw (unredacted).
    3. A file with many distant hunks stops emitting `:` lines after `MAX_EXPANDED_CONTEXT_LINES_PER_FILE` (`120`) expanded lines while keeping all raw diff hunks intact.
    4. Pure-deletion hunk (`@@ -10,2 +10,0 @@`) still expands surrounding lines anchored around line 10 without panicking or losing hunk headers.
- In `crates/review/src/tools.rs`:
  - `load_expanded_diffs_skips_added_oversized_and_secret_files`:
    - Modified `src/a.rs` is expanded; `FileStatus::Added` `src/new.rs` is skipped; `.env` is skipped (`is_secret_path`); file whose raw `diff_block` is `>= MAX_EXPAND_FILE_CHARS` is skipped.
- In `crates/review/tests/engine.rs` (`finder_still_sees_the_raw_diff` + new assertion in `agentic_generalist_uses_tools_and_verifies`):
  - Assert that a cloned modified file gets `:` surrounding lines in `<diffs>`, while `small_pr_skips_clone_and_tools_but_still_verifies` has no `:` expanded lines.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p adus-review -j 2 -- expand_diff_block load_expanded_diffs`
Expected: FAIL (`expand_diff_block` and `load_expanded_diffs` not defined).

- [ ] **Step 3: Implement expanded hunk context & workspace loading**

- `crates/review/src/prompt.rs`: implement `expand_diff_block(file, head_text)` per Spec §8.1; update `batch_targets` and `build_finder_user_prompt` to take `expanded: &HashMap<String, String>` and look up `normalize_path(&file.path)`. Update existing `prompt.rs` unit tests for the new `expanded` parameter (`&HashMap::new()`).
- `crates/review/src/tools.rs`: implement `pub(crate) async fn load_expanded_diffs(ws: Arc<RepoWorkspace>, files: &[FileDiff]) -> HashMap<String, String>` running in `spawn_blocking` per Spec §8.2.
- `crates/review/src/finder.rs`: add `pub expanded: &'a HashMap<String, String>` to `FinderInput<'a>` and pass it to `build_finder_user_prompt`.
- `crates/review/src/engine.rs`: move `batch_targets` after `clone_pr` in `review_targets`, compute `expanded = match &workspace { Some(ws) => load_expanded_diffs(Arc::clone(ws), &files).await, None => HashMap::new() }`, and pass `&expanded` to `batch_targets` and `FinderInput`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p adus-review -j 2`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/prompt.rs crates/review/src/tools.rs crates/review/src/finder.rs crates/review/src/engine.rs crates/review/tests/engine.rs
git commit -m "feat(review): pre-seed 15-line redacted hunk context in Finder when clone exists"
```

---

### Task 6: Sync `docs/SPEC.md`, mark spec implemented, and run full verification

**Files:**
- Modify: `docs/SPEC.md` (§8.3, §8.4, §10)
- Modify: `docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md` (`Status: implemented`)

**Interfaces:**
- Consumes: Final implemented behavior, constants, and stage behavior from Tasks 1–5.

- [ ] **Step 1: Update `docs/SPEC.md` and mark spec implemented**

- Update `docs/SPEC.md` §8.3, §8.4, and §10 per Spec §11.
- Mark `docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md` header `Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §8.3–§8.4)`.

- [ ] **Step 2: Run full workspace checks**

Run:
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace -j 2`

Expected: All pass with 0 warnings and 0 failures.

- [ ] **Step 3: Commit**

```bash
git add docs/SPEC.md docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md docs/superpowers/plans/2026-11-24-agentic-prompt-and-harness.md
git commit -m "docs: sync SPEC.md for agentic prompt redesign and review harness improvements"
```

---

## Amendment §12: Finder Reports, Verifier Filters

**Goal:** Turn the Finder from a second filter into a reporter (promote code-backed suspicions, dismiss only with a reason, log every dismissal) and narrow the Verifier's "intended behaviour" drop to intent the repository itself states.

**Architecture:** Prompt text changes in `prompt.rs`, `specialists.rs` and `verifier.rs`, plus an optional `dismissed` list on `submit_findings` that `parse.rs` parses (capped, cut, redacted) and `SubmitFindings::parse_arguments` logs at debug level on the accepted call only. `SubmitSpec::Output` stays `Vec<Suggestion>`; no change to `agent.rs`, `engine.rs`, the DB, the API or `web/`.

**Spec:** [docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md](../specs/2026-11-24-agentic-prompt-and-harness-design.md) §12

### Amendment Constraints

- The **Clean-room and no-assumption rule** in Global Constraints applies unchanged to Tasks 7–10; every implementer and reviewer prompt carries it verbatim. Kodus files consulted for §12 are listed in spec §12.7.
- Exact values: `MAX_DISMISSED = 30`, `DISMISSED_TEXT_CHARS = 300` (cut by `char`), log messages `"finder dismissed"` (one per entry) and `"finder dismissals"` (count field `dismissed`), Finder phase-3 heading `Promote or dismiss`.
- The field is named in prompts only as `"dismissed"` (double quotes, matching `FINDER_FIELDS` style) and only in tool-enabled prompts. `GENERALIST_ROLE` and every diff-only prompt never contain `"dismissed"`.
- Prefix-cache invariant (§4.1), specialist opening sentences, the `"call submit_findings."` / `"respond with the JSON array."` endings, and the Verifier invariants listed in Global Constraints stay intact.
- **Precondition:** the live-trace work and the TEMP `rereview` command are in the user's stash (`wip: live agent TRACE debugging`) and are not part of these tasks; never pop, drop or stage them. `.cargo/config.toml` is the user's local edit and is never staged. Task 7's logs name the agent through fields on `SubmitFindings`, not through a tracing span.

### Amendment Review Focus

1. Model sends `dismissed` as an object, a string, or not at all → the submission is accepted with its findings unchanged and no dismissals (pinned in Task 7).
2. A dismissal with a blank `why_safe` (`"   "`) or a negative `line` → the blank one is skipped; the negative line keeps the entry with `line: None` (pinned in Task 7).
3. Multi-byte text (`"é"` × 400) in `suspicion` → cut to 300 chars without panicking (pinned in Task 7).
4. A token (`ghp_…`) inside `why_safe` → returned as `[REDACTED]`, so it never reaches the logs (pinned in Task 7).
5. Diff-only Finder prompts (all five roles) never mention `"dismissed"`, while tool prompts do (pinned in Task 8).

---

### Task 7: `dismissed` on `submit_findings` — schema, parser, logging

**Files:**
- Modify: `crates/review/src/parse.rs` (`submit_findings_tool_def` ~145-200; new items after `parse_submit_findings` ~209; test `submit_findings_tool_def_uses_portable_schema` ~486)
- Modify: `crates/review/src/finder.rs` (`SubmitFindings::parse_arguments` ~276-287, imports ~27-30)
- Modify: `crates/review/src/prompt.rs` (`build_finder_system_prompt` ~214-232, new const next to `FINDER_FIELDS`)

**Interfaces:**
- Produces (in `parse.rs`): `pub(crate) const MAX_DISMISSED: usize = 30;`, `pub(crate) const DISMISSED_TEXT_CHARS: usize = 300;`, `#[derive(Debug, Clone, PartialEq, Eq)] pub(crate) struct Dismissal { pub file: String, pub line: Option<u32>, pub suspicion: String, pub why_safe: String }`, `pub(crate) fn parse_dismissed(args: &Value) -> Vec<Dismissal>`.
- Produces (in `prompt.rs`): `const FINDER_DISMISSED_TOOLS: &str`, appended to the tool-enabled `<Output>` only.

- [ ] **Step 1: Write the failing tests** in `parse.rs`:

```rust
// Extend submit_findings_tool_def_uses_portable_schema:
assert_eq!(params["required"], json!(["findings"])); // unchanged: dismissed is optional
let dismissed = &params["properties"]["dismissed"];
assert_eq!(dismissed["type"], json!("array"));
assert_eq!(dismissed["items"]["required"], json!(["file", "suspicion", "why_safe"]));
assert_eq!(dismissed["items"]["properties"]["line"]["type"], json!("integer"));

#[test]
fn parse_dismissed_skips_malformed_caps_cuts_and_redacts() {
    let ok = |file: &str| json!({"file": file, "suspicion": "s", "why_safe": "w"});
    let args = json!({"findings": [], "dismissed": [
        {"file": "a.go", "line": 12, "suspicion": "nil deref", "why_safe": "guarded at a.go:10"},
        ok("b.go"),
        {"file": "c.go", "line": -3, "suspicion": "s", "why_safe": "w"},
        {"file": "d.go", "suspicion": "s", "why_safe": "   "},
        {"file": "e.go", "suspicion": "s"},
        "not an object",
        {"file": "f.go", "suspicion": "é".repeat(400), "why_safe": "token ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa leaked"},
    ]});
    let got = parse_dismissed(&args);
    assert_eq!(got.len(), 4);
    assert_eq!(got[0], Dismissal { file: "a.go".into(), line: Some(12), suspicion: "nil deref".into(), why_safe: "guarded at a.go:10".into() });
    assert_eq!(got[1].line, None);
    assert_eq!((got[2].file.as_str(), got[2].line), ("c.go", None));
    assert_eq!(got[3].suspicion.chars().count(), DISMISSED_TEXT_CHARS);
    assert!(got[3].why_safe.contains("[REDACTED]") && !got[3].why_safe.contains("ghp_"));

    for absent in [
        json!({"findings": []}),
        json!({"findings": [], "dismissed": {"file": "x"}}),
        json!({"findings": [], "dismissed": "x"}),
    ] {
        assert!(parse_dismissed(&absent).is_empty());
    }
    let many: Vec<Value> = (0..40).map(|i| ok(&format!("f{i}.go"))).collect();
    assert_eq!(parse_dismissed(&json!({"dismissed": many})).len(), MAX_DISMISSED);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- parse_dismissed submit_findings_tool_def`
Expected: compile error (`parse_dismissed`, `Dismissal`, `MAX_DISMISSED` not defined).

- [ ] **Step 3: Implement in `parse.rs`**

- `submit_findings_tool_def`: add the optional `dismissed` array (items: `file` string, `line` integer, `suspicion` string, `why_safe` string; `required: ["file", "suspicion", "why_safe"]`; portable subset only) and extend the tool description to ask for every ruled-out suspicion with the reason it cannot fail.
- `parse_dismissed`: read `args["dismissed"]` as an array (anything else → empty); for each object take trimmed `file`, `suspicion`, `why_safe` (skip the item when any is missing, non-string or blank); `line` via `as_u64` → `u32::try_from` (else `None`); cut `suspicion`/`why_safe` to `DISMISSED_TEXT_CHARS` chars, then `crate::redact::redact_secrets(..).into_owned()`; stop after `MAX_DISMISSED` kept entries.

- [ ] **Step 4: Log on the accepted call in `finder.rs`**

Add `specialist: &'static str` and `batch: usize` to `SubmitFindings` (set from `kind.label()` and `index` where `run_all` builds it per agent; the test construction in `finder_bounces_off_line_anchor_once_then_accepts_partial_on_second_or_last_step` uses `"Generalist"` and `0`). In `SubmitFindings::parse_arguments`, after the anchor check has passed (just before `Ok(suggestions)`), call `parse_dismissed(arguments)` and emit `tracing::debug!(specialist = self.specialist, batch = self.batch, file = %d.file, line = ?d.line, suspicion = %d.suspicion, why_safe = %d.why_safe, "finder dismissed")` per entry, then `tracing::debug!(specialist = self.specialist, batch = self.batch, dismissed = list.len(), "finder dismissals")`. The early `?` return on a bounced call must leave this code unreached.

- [ ] **Step 5: Describe the field in the tool prompt (`prompt.rs`)**

Add `FINDER_DISMISSED_TOOLS` (one short paragraph: optional top-level `"dismissed"` list of `{"file", "line" (optional), "suspicion", "why_safe"}` for every suspicion ruled out, with the reason it cannot fail; never posted) and use `format!("{FINDER_OUTPUT_TOOLS}\n{FINDER_FIELDS}\n{FINDER_DISMISSED_TOOLS}")` in the `with_tools` branch of `build_finder_system_prompt`. The diff-only branch is unchanged.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib -- parse:: finder:: prompt::`
Expected: PASS, including the existing `finder_bounces_off_line_anchor_once_then_accepts_partial_on_second_or_last_step` and `specialists_share_byte_identical_system_prefix`.

- [ ] **Step 7: Commit**

```bash
git add crates/review/src/parse.rs crates/review/src/finder.rs crates/review/src/prompt.rs
git commit -m "feat(review): log Finder dismissals from an optional submit_findings field"
```

---

### Task 8: Finder stance and specialist roles

**Files:**
- Modify: `crates/review/src/prompt.rs` (`FINDER_MINDSET`, `FINDER_WORKFLOW_TOOLS`, `FINDER_WORKFLOW_DIFF_ONLY`, `FINDER_SCOPE`, `FINDER_SEVERITY` ~150-200; test `specialists_share_byte_identical_system_prefix` ~822-871)
- Modify: `crates/review/src/specialists.rs` (`GENERALIST_ROLE`, `BUG_ROLE`, `BUG_ROLE_DIFF_ONLY` ~18-58; test `active_specialists_omits_rules_when_none_match` ~160-180)

**Interfaces:**
- Consumes: `FINDER_DISMISSED_TOOLS` and the `"dismissed"` field name from Task 7.

- [ ] **Step 1: Write the failing tests**

```rust
// prompt.rs, end of specialists_share_byte_identical_system_prefix:
for phrase in ["Promote or dismiss", "\"dismissed\"", "newly reachable", "not a test"] {
    assert!(tools.contains(phrase), "{phrase}");
}
for gone in ["leave it out", "Second-guessing an intentional change", "Many pull requests have no defects"] {
    assert!(!tools.contains(gone), "{gone}");
}
for kind in ALL_KINDS {
    let plain = build_finder_system_prompt(kind, false);
    assert!(plain.contains("Promote or dismiss"), "{kind:?}");
    assert!(!plain.contains("\"dismissed\""), "{kind:?}");
}

// specialists.rs, in active_specialists_omits_rules_when_none_match:
assert!(SpecialistKind::Generalist.role_block().contains("dismissed with a stated reason"));
assert!(!SpecialistKind::Generalist.role_block().contains("submit []"));
for with_tools in [true, false] {
    assert!(SpecialistKind::Bug.role_block_for(with_tools).contains("promoting or dismissing"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p adus-review -j 2 --lib -- specialists_share active_specialists_omits`
Expected: FAIL on `Promote or dismiss`.

- [ ] **Step 3: Rewrite the Finder constants per spec §12.3**

`FINDER_MINDSET` (a real team PR, "not a test" wording, surface every defect the change introduces or makes reachable, empty only when every suspicion was dismissed with a reason); phase 3 of both workflows headed `Promote or dismiss` (the tool version names `"dismissed"`, the diff-only version names neither the field nor any tool); `FINDER_SCOPE` items as listed in §12.3 (the `<pr_description>` goal-not-correctness rule, "newly reachable" older defects, failure-not-question wording, report-and-let-the-Verifier-filter instead of "leave it out"); `FINDER_SEVERITY` `info` line without "verified". Keep phase 4's "do not keep searching just because steps remain".

- [ ] **Step 4: Update the roles per spec §12.4**

`GENERALIST_ROLE` step 4: "Submit once every hunk is checked: each suspicion is either a finding or dismissed with a stated reason." `BUG_ROLE` and `BUG_ROLE_DIFF_ONLY` step 2 end with "…before promoting or dismissing it" (the diff-only one still names no tool). Opening sentences unchanged.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p adus-review -j 2`
Expected: PASS, including the role matchers in `crates/review/tests/engine.rs`.

- [ ] **Step 6: Commit**

```bash
git add crates/review/src/prompt.rs crates/review/src/specialists.rs
git commit -m "feat(review): Finder promotes code-backed suspicions and dismisses only with a reason"
```

---

### Task 9: Verifier drops for repository-stated intent only

**Files:**
- Modify: `crates/review/src/verifier.rs` (`VERIFIER_DROP_RULES` ~507-520; test `user_prompt_puts_shared_sections_first_and_the_candidate_last` ~1234-1275)

- [ ] **Step 1: Write the failing test** (extend `user_prompt_puts_shared_sections_first_and_the_candidate_last`, after `let system = build_system_prompt(true);`):

```rust
assert!(!system.contains("second-guesses the change's stated goal"));
assert!(system.contains("the repository itself states"));
assert!(system.contains("never by itself grounds to drop"));
assert!(system.contains("security hole, data loss or crash"));
assert!(system.contains("makes it reachable or worse"));
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p adus-review -j 2 --lib -- user_prompt_puts_shared_sections_first`
Expected: FAIL on the first assertion.

- [ ] **Step 3: Edit `VERIFIER_DROP_RULES` per spec §12.6**

Remove the `<pr_description>` second-guess rule; add the repository-stated-intent rule (a code comment, a doc beside the code, or a test that pins it; "the repository itself states"; never for a "security hole, data loss or crash"); add "`<pr_description>` … is context, never by itself grounds to drop"; add the keep guidance for edge-case triggers and older code this change "makes it reachable or worse". Keep the `<memories>`, hedge, Rules-reviewer, `drop_suggestion` and keep-by-default lines, including the exact `Findings reported by the Rules reviewer (`reported_by`)` opening.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p adus-review -j 2 --lib -- verifier::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/review/src/verifier.rs
git commit -m "feat(review): Verifier drops for repository-stated intent only, not the PR description"
```

---

### Task 10: Sync `docs/SPEC.md` and mark spec §12 implemented

**Files:**
- Modify: `docs/SPEC.md` (§8.4 stages 1 and 3, §10 entry)
- Modify: `docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md` (status line)

- [ ] **Step 1: Readjust `docs/SPEC.md` per spec §12.8**

§8.4 stage 1: replace "anti-CTF clean-PR stopping rule with `[]` early exit, … and do-not-report list" with the report-and-dismiss stance and the `dismissed` list (`MAX_DISMISSED = 30`, `DISMISSED_TEXT_CHARS = 300`, redacted, debug-logged on the accepted call, never posted or persisted). §8.4 stage 3: replace "`<pr_description>`-stated PR intent" with the repository-stated-intent rule (never for security, data loss or crash) and "`<pr_description>` alone is never grounds to drop". §10: replace "**§12 amendment pending** … until it lands" with "§12 amendment: Finder reports / Verifier filters, `dismissed` log".

- [ ] **Step 2: Mark the spec implemented**

Status line: `Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §8.3–§8.4, including the §12 amendment)`.

- [ ] **Step 3: Run full workspace checks**

Run: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace -j 2`
Expected: all pass, 0 warnings.

- [ ] **Step 4: Live check (manual, by the user)**

Re-run the review of `manot40/better-booru#13` with `RUST_LOG=info,adus_review=debug,adus_review::trace=trace`. Expected (spec §12.1): each of the four defects appears as a candidate or as a `"finder dismissed"` line with a reason, and the Verifier keeps at least three.

- [ ] **Step 5: Commit**

```bash
git add docs/SPEC.md docs/superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md docs/superpowers/plans/2026-11-24-agentic-prompt-and-harness.md
git commit -m "docs: sync SPEC.md for the Finder-reports / Verifier-filters amendment"
```
