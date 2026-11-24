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
