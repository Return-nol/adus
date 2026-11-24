# Agentic Prompt Redesign & Review Harness Improvements — Design Specification

Date: 2026-11-24 · Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §8.3–§8.4)

> **Binding scope & policy:** Extends [docs/SPEC.md](../../SPEC.md) §8.3–§8.4 and the agentic review engine spec ([2026-11-21-agentic-review-engine-design.md](2026-11-21-agentic-review-engine-design.md)). The clean-room and no-assumption rule in `AGENTS.md` applies: Kodus community code may be read as a behaviour reference only (cite it), Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened. All prompts, schemas, and code are written fresh for Adus.

---

## 1. Intent & Why

Live tracing on `manot40/better-booru` (PRs #11 and #12) and `manot40/adus` (PR #2) using `deepseek/deepseek-v4.1-flash-1017` exposed six concrete failure modes in the current prompt + review harness:

1. **CTF / "planted bug" hunting loop in Finder.** `FINDER_MINDSET` and `FINDER_WORKFLOW_TOOLS` presuppose defects exist (*"Hunt thoroughly... find the real defects this change introduces"*), with no explicit rule that a clean PR submitting `[]` early is normal and desirable. In the live trace on `better-booru#12` (8 files, +102 / -13), Generalist verified all changed files by Step 6, yet burned 13 more steps reasoning *"Where's the planted bug?"* and *"I'm now struggling to find a real defect"*, hitting `wrap_up="steps"` at Step 19 (~598K input tokens) and inventing a speculative *"Please verify that..."* warning at wrap-up. In Deep mode (`#11`), the same loop caused `Bug` to time out.
2. **Compaction wiping `read_file` outputs mid-review.** OpenRouter/OpenAI-compatible `provider_meta` reasoning pushes conversation size past `DEFAULT_COMPACTION_CHAR_LIMIT = 160_000` around Step 12. `compact()` replaces old `ToolResult`s with `"[earlier tool output omitted to save context]"`, forcing the agent to re-read files it already opened (Steps 14 and 17 in the trace) and invalidating prefix caching.
3. **Verifier blindness to PR intent, `<memories>` contradictions, hedges, and bad code suggestions.** `verifier::build_user_prompt` omits `<pr_title>`, `<pr_description>`, and `<pr_manifest>`. On `better-booru#12`, Verifier kept a `warning` questioning whether returning local regeneration URLs for orphaned rows was acceptable even though `<pr_description>` explicitly stated that was the PR's goal. Verifier also lacks explicit `<Verdict>` drop rules for `<memories>` contradictions and *"please verify / consider"* hedges, and cannot strip a broken `suggestion` patch without dropping an otherwise valid finding.
4. **Specialist prompts lack tool playbooks, and `Rules` misses `<repository_instructions>`.** `specialists.rs` lists defect topics rather than tool-driven investigation steps, neither Finder nor Verifier tells models they can batch up to 4 read-only tool calls in parallel per turn, and `active_specialists` omits `SpecialistKind::Rules` when `batch.rules` is empty even if `repo_instructions` (`AGENTS.md`, `CLAUDE.md`, `.cursor/rules/*.mdc`) is present.
5. **Silent post-submit anchor drops.** `keep_valid_batch` silently discards findings anchored outside `+` lines with no logging and no chance for the agent to re-anchor while steps remain.
6. **Discovery turns wasted reading changed files.** Unified Git diffs carry only 3 lines of hunk context, forcing Finder to spend Steps 1–3 calling `read_file` on files already listed in `<diffs>` before investigating cross-file callers.

**Success:** Clean PRs terminate early with `[]` without manufacturing hedges; specialists execute focused, parallel tool investigations and own `<repository_instructions>`; compaction preserves excerpts instead of triggering re-reads; Verifier sees PR intent, drops `<memories>`/`<pr_description>`-contradicted or hedged candidates, and can strip bad suggestions; off-line anchors get one bounce when steps remain; and cloned Finders see ±15 lines of redacted surrounding context from Turn 0.

---

## 2. Approaches Considered

| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **A — Prompt-only redesign** | Rewrite `prompt.rs`, `specialists.rs`, `verifier.rs`, `dedup.rs` strings only. | Fastest, lowest diff, but leaves `compact()` re-reads, Verifier `<pr_description>` blindness, `Rules` + `AGENTS.md` gap, silent off-line drops, and 3-line diff blindness unfixed. | Rejected |
| **B — Prompt redesign + targeted harness fixes + expanded hunk context** | Redesign role prompts + fix compaction, step pacing, `Rules` activation, `submit_findings` anchor bounce, Verifier PR-intent/`drop_suggestion`, and ±15-line pre-seeded hunk context inside `crates/review`. | Directly eliminates every failure mode proven by the trace with zero DB migrations, API changes, or UI changes. | **Selected** |
| **C — Full AST graph + DB/UI funnel persistence** | Add Kodus-style persistent AST call graph, external linters/sandboxes, and Postgres/UI funnel telemetry. | Violates Adus's 3-container / no-sandbox / no-AST-DB footprint ([SPEC.md §1–2](../../SPEC.md#1-overview--product-decisions)) before measuring Approach B. | Rejected |

---

## 3. Goals & Non-Goals

### Goals

1. **Role prompts, specialist playbooks, `Rules` activation & Dedup tie-breaking** (§4): anti-CTF clean-PR stopping rule, parallel tool batching, role-specific investigation workflows, `Rules` spawned when `!batch.rules.is_empty() || input.repo_instructions.is_some()`, and deterministic duplicate preference in Dedup.
2. **Preview-preserving compaction & neutral halfway pacing** (§5): `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000`, `400`-char excerpt + marker instead of a blank placeholder, and a halfway pacing note that discourages extra steps once checks are complete.
3. **`submit_findings` anchor validation & drop observability** (§6): validate normalized batch paths and `+` line numbers on `submit_findings` with at most one retry bounce when `tally.steps_used < config.max_steps`, plus `filtered_dropped` in `review stages finished`.
4. **Verifier PR-intent context, verdict rules & `drop_suggestion`** (§7): `<pr_title>`, `<pr_description>`, `<pr_manifest>` in Verifier's cacheable prefix; explicit `<memories>`, `<pr_description>`, and hedge drop rules; optional `drop_suggestion: bool` on `submit_verdict`.
5. **Pre-seeded expanded hunk context** (§8): when a `RepoWorkspace` exists (not diff-only / Small PR), widen modified/renamed hunk context up to ±15 lines marked `:` (secret-redacted outside raw hunks, bounded per file, leaving `CommentableRange` strictly anchored to raw `+` lines).

### Non-Goals

- Persisted AST call graphs, sandbox containers, or running compilers/linters.
- Database migrations, REST API endpoints, `web/` UI changes, or changes to `crates/llm` `provider_meta` replay.
- Replacing the untracked draft `docs/superpowers/specs/2026-11-24-finder-completion-gate-and-verifier-effort-design.md`: §5 uses the exact same compaction constants (`320_000`, `400`, `"\n[older tool output shortened to save context]"`) so both specs compose cleanly if both land.

---

## 4. Role Prompts, Specialist Playbooks & `Rules` Activation

### 4.1 Prefix-Cache Invariant (`prompt.rs`)

`build_finder_system_prompt(kind, with_tools)` continues to place `<ReviewInstructions>` first (byte-identical across all 5 `SpecialistKind` values for a given `with_tools`) and `<Role>` last.

### 4.2 Shared Finder System Instructions (`prompt.rs`)

- **`FINDER_MINDSET`:** Keep the `@adudu` senior-engineer framing, but state explicitly that **many PRs have no defects**: submitting `[]` as soon as every changed hunk has been checked is a normal, good outcome — never invent, stretch, or pad findings to fill a list.
- **`FINDER_WORKFLOW_TOOLS`:** Replace the 3-phase list with a 4-phase loop:
  1. **Sweep every changed hunk:** read all files in `<diffs>` before zooming in on any single hypothesis.
  2. **Investigate in parallel:** batch up to 4 independent `read_file` / `grep` / `list_dir` / `find_files` calls in a single turn to verify callers, type/contract invariants, upstream validation, migrations/schemas, and error paths. Do not re-read lines already seen unless a new question requires it.
  3. **Verify or discard:** keep a suspicion only when concrete code evidence proves a failure path or rule violation; drop speculative concerns.
  4. **Submit immediately:** once every changed hunk is checked, call `submit_findings` at once (with `[]` if clean). Do not keep searching just because steps remain.
- **`FINDER_WORKFLOW_DIFF_ONLY`:** Same hunk-sweep, evidence-bar, and clean-PR early-exit framing without naming any repository tools (`read_file`, `grep`, `list_dir`, `find_files`, `submit_findings` remain absent).
- **`FINDER_SCOPE`:** Preserve existing `+`/context/manifest/rules/memories/repo-instructions rules and add explicit calibration:
  - **`:` context lines:** lines marked `:` in `<diffs>` are expanded surrounding context lines outside the diff hunk; use them to understand surrounding code, **never** as `line` or `start_line` targets (only `+` lines in diff hunks accept findings).
  - **Do-not-report list:**
    - Speculative hedges (*"please verify that X is acceptable"*, *"consider checking Y"*, *"ensure Z"*);
    - Second-guessing an intentional change explicitly explained in `<pr_description>` unless the code introduces an unintended defect or violates `<rules>`/`<repository_instructions>`;
    - Hypotheticals already prevented by types, DB constraints, or upstream guards;
    - Unrequested style, naming, comments, logging, or missing tests unless asked for by `<rules>` or `<repository_instructions>`.
- **`FINDER_SEVERITY` & `FINDER_FIELDS`:** Require `body` to state the concrete failure trigger, verified symbol/file path, and fix (not vague advice).

### 4.3 Specialist Playbooks & `Rules` Spawning (`specialists.rs`, `finder.rs`)

Keep each specialist's opening sentence unchanged (preserving integration-test role markers `"You are the generalist reviewer"`, `"You are the correctness reviewer"`, `"You are the security reviewer"`, `"You are the performance reviewer"`, `"You are the team-rules reviewer"`), and expand each `<Role>` block into a concrete investigation playbook:

- **`Generalist`:** Sweep all hunks first; trace modified callers/schemas/migrations and error-return paths with `grep`/`read_file`; check matching `<rules>` and `<repository_instructions>`; stop and submit `[]` as soon as all hunks pass.
- **`Bug`:** Focus on state transitions, boundary/empty/zero/Unicode inputs, swallowed errors, partial updates, resource/file leaks, stale index/cache state, migration/ORM mismatches, concurrency/races, and caller/callee contract drift across files (`grep` callers and sibling implementations when `with_tools = true`; `SpecialistKind::role_block_for(with_tools)` uses `BUG_ROLE_DIFF_ONLY` when `!with_tools` so `Bug` diff-only prompts stay free of tool names). Leave security, performance, and unrequested style to others.
- **`Security`:** Trace untrusted inputs from entrypoint/request parsing through authn/authz/validation middleware to sinks (SQL/shell/template/path/SSRF/deserialization/crypto/secrets/error leaks). Check tenant/object-ownership bounds and fail-open auth guards.
- **`Performance`:** Verify algorithmic complexity and call-site scale before reporting: check whether loops perform N+1 queries or network I/O, whether SQL predicates are non-sargable on large tables or queries lack limits, whether unbounded buffers/collections grow with input, or whether blocking I/O runs on async/hot paths. Ignore micro-optimizations on small bounded data.
- **`Rules`:** Check every changed hunk strictly against matching `<rules>` and `<repository_instructions>`; verify exemptions and local patterns before flagging; cite the exact rule or instruction in `body` and use the rule's severity.
- **Activation (`specialists.rs` & `finder.rs`):** Rename the boolean parameter in `active_specialists(mode: ReviewMode, has_rules_or_instructions: bool) -> Vec<SpecialistKind>`. In `finder.rs::run_all`, pass `!batch.rules.is_empty() || input.repo_instructions.is_some()` so `Rules` always runs in `Specialists` mode when `<repository_instructions>` is loaded, even with zero DB rules.

### 4.4 Dedup Tie-Breaking (`dedup.rs`)

Update `DEDUP_SYSTEM` Task 1 so when merging duplicates for the same root cause, Dedup prefers (in order): (1) higher severity, (2) the finding with concrete file/symbol evidence and tighter line anchor, and (3) a rule/instruction-citing finding when applicable. Keep the JSON output contract, `already_raised` rules, and `DEDUP_MAX_TOKENS` unchanged.

---

## 5. Preview-Preserving Compaction & Step Pacing (`agent.rs`)

### 5.1 Compaction Constants & Behavior

- `DEFAULT_COMPACTION_CHAR_LIMIT: usize = 320_000` (up from `160_000`, accommodating verbose `provider_meta` reasoning without evicting early `read_file` outputs).
- `COMPACT_EXCERPT_CHARS: usize = 400`
- `COMPACTED_MARKER: &str = "\n[older tool output shortened to save context]"`
- Remove `OMITTED_TOOL_OUTPUT` and `COMPACT_MIN_CHARS`.
- In `compact(messages, limit)`: while `total > limit`, scan tool results outside index `0` and the last `COMPACT_KEEP_TAIL` (`4`) messages, oldest first:
  - Skip any result with `len <= COMPACT_EXCERPT_CHARS + COMPACTED_MARKER.chars().count()` (446 chars) or already ending with `COMPACTED_MARKER`.
  - Otherwise replace `result.content` using `truncate_with_marker(&result.content, COMPACT_EXCERPT_CHARS, COMPACTED_MARKER)` and subtract the saved chars from `total`.

### 5.2 Halfway Pacing Note

Update the halfway note in `run_react_loop` (`tally.steps_used == config.max_steps / 2` when `with_repo_tools && !wrap_up.active() && config.max_steps >= BUDGET_NOTE_MIN_STEPS`):

```text
Step budget: {} of {} steps used. Batch remaining lookups in parallel, and call {submit_name} as soon as your checks are complete — do not spend extra steps once you have enough evidence.
```

---

## 6. `submit_findings` Anchor Validation & Drop Observability

### 6.1 `SubmitSpec::parse_arguments` Signature (`agent.rs`, `verifier.rs`, `finder.rs`)

Change `SubmitSpec::parse_arguments` to:

```rust
fn parse_arguments(&self, arguments: &Value, can_retry: bool) -> Result<Self::Output, String>;
```

In `run_react_loop`, `tally.add(&response)` has already incremented `tally.steps_used` for the current turn, so pass `can_retry = tally.steps_used < config.max_steps`. (`SubmitAnswer` in `agent.rs` tests and `SubmitVerdict` in `verifier.rs` ignore `_can_retry`.)

### 6.2 Batch Anchor Validation (`parse.rs`, `finder.rs`)

- Add `pub(crate) fn validate_batch_anchors(suggestions: &[Suggestion], allowed_by_file: &HashMap<String, &CommentableRange>) -> Result<(), String>` in `parse.rs`:
  - Checks each `Suggestion` in order:
    1. `normalize_path(&s.file)` must exist in `allowed_by_file`; if not, return an error naming the invalid file and listing the batch's valid file paths.
    2. `allowed.lines.contains(&s.line)` must hold; if not, return an error naming the file and invalid `line` and listing up to 5 nearest valid `+` line numbers from `allowed.lines` (sorted by `abs_diff(s.line)`, ties preferring smaller line numbers, rendered in ascending order).
- Add `pub allowed: &'a HashMap<String, CommentableRange>` to `FinderInput<'a>` (`finder.rs`, populated in `engine.rs::review_targets`).
- Build a per-batch `SubmitFindings` struct holding the batch's `HashMap<String, &'a CommentableRange>` and `anchor_retried: AtomicBool` (`false`).
- In `SubmitFindings::parse_arguments(&self, arguments: &Value, can_retry: bool)`:
  1. Parse `suggestions = parse_submit_findings(arguments)?`.
  2. If `can_retry && !self.anchor_retried.swap(true, Ordering::Relaxed)`, call `validate_batch_anchors(&suggestions, &self.allowed)` — on `Err(msg)`, return `Err(msg)` so `answer_call` sends the correction prompt back to the agent for one re-anchor turn.
  3. On `can_retry == false` or on the second `submit_findings` attempt, return `Ok(suggestions)` so valid findings are never discarded into `AgentError::Unparseable`, and `keep_valid_batch` cleans up any remaining invalid entry.

### 6.3 Filtered-Drop Logging (`engine.rs`)

In `collect_candidates`, track `filtered_dropped: usize` (sum over finished Finder tasks of `raw_count - valid_count` across `keep_valid_batch` before `dedup_exact`) and return `(Vec<CandidateFinding>, usize)`. Log `filtered_dropped` in `review stages finished`.

---

## 7. Verifier PR-Intent Context, Verdict Rules & `drop_suggestion`

### 7.1 Shared PR Description Helper (`prompt.rs`)

Extract `pub(crate) fn format_pr_description(description: &str) -> String` in `prompt.rs` using `truncate_with_marker(description, MAX_DESCRIPTION_CHARS, "\n[truncated]")`, and reuse it in `build_finder_user_prompt` and `verifier::build_user_prompt`.

### 7.2 PR Context in Verifier (`verifier.rs`, `engine.rs`)

- Add `pub pr: &'a PullRequest` and `pub manifest: &'a str` to `VerifyInput<'a>` and `Evidence<'a>`. Pass `s.pr` and `manifest` from `engine.rs::review_targets`.
- In `verifier::build_user_prompt`, prepend `<pr_title>`, `<pr_description>`, and `<pr_manifest>` **before** `<rules>`, `<memories>`, `<repository_instructions>`, `<diff>`, `<code_context>`, `<candidate>` (keeping all PR-wide and per-file blocks in the shared cacheable prefix ahead of `<candidate>`).
- Update `VERIFIER_UNTRUSTED_INPUT` to list `<pr_title>`, `<pr_description>`, and `<pr_manifest>` alongside `<candidate>`, `<rules>`, `<memories>`, `<repository_instructions>`, `<diff>`, and `<code_context>`.
- Update `VERIFIER_WORKFLOW_TOOLS` to state the Verifier may batch up to 4 independent tool calls in one turn.

### 7.3 Updated `<Verdict>` Rules & `drop_suggestion` (`verifier.rs`)

- Extend `VERIFIER_DROP_RULES` with three explicit drop conditions (keeping fail-closed and never-rewrite-prose invariants):
  - **Team memories:** drop when `<memories>` records a team decision/exception that explicitly permits or explains the flagged pattern.
  - **PR intent:** drop when the finding only second-guesses the change's stated goal in `<pr_description>` without demonstrating an unintended defect, regression, or rule violation.
  - **Hedges:** drop speculative *"please verify / consider / check whether"* comments that fail to prove an actual bug or risk.
- Add guidance for `drop_suggestion`: when `keep = true` and `<candidate>` includes a non-null `suggestion`, check the replacement code against `<diff>` and `<code_context>`; if the replacement is syntactically broken, misses lines in `start_line..=line`, or contradicts the explanation, set `"drop_suggestion": true` (keeping the finding without the broken patch).
- Update `SubmitVerdict`:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub(crate) struct Verdict { pub keep: bool, pub drop_suggestion: bool }`
  - `SubmitVerdictArgs { keep: bool, #[serde(default)] drop_suggestion: bool }` (so omitting `drop_suggestion` defaults to `false`).
  - `submit_verdict_tool_def` adds optional `"drop_suggestion": {"type": "boolean", "description": "..."}` (not added to `"required"`, preserving backward compatibility).
  - `Outcome::Kept { drop_suggestion: bool }`; in `summarize`, when `drop_suggestion` is `true`, clear `candidate.suggestion.suggestion = None` and `candidate.suggestion.start_line = None` before pushing to `report.kept`.

---

## 8. Pre-Seeded Expanded Hunk Context (`prompt.rs`, `tools.rs`, `engine.rs`)

### 8.1 Constants & Expansion Rules (`prompt.rs`)

- `pub(crate) const EXPANDED_HUNK_CONTEXT: u32 = 15;`
- `pub(crate) const MAX_EXPAND_FILE_CHARS: usize = 20_000;`
- `pub(crate) const MAX_EXPANDED_CONTEXT_LINES_PER_FILE: usize = 120;`
- `pub(crate) fn expand_diff_block(file: &FileDiff, head_text: &str) -> String`:
  - Falls back to `diff_block(file)` if `file.patch` is empty/absent, has no valid hunks, or `head_text` is empty.
  - Parses hunk line numbers from `file.patch` (using `hunk_right_start` and tracking each hunk's `first..=last` right-side range; hunks with 0 right-side lines — pure deletions — track the right-side cursor `start` so surrounding context before/after the deletion can still be shown using `start` as the anchor).
  - For each hunk, inserts up to `EXPANDED_HUNK_CONTEXT` lines immediately before the hunk (ending at `hunk_first - 1`) and up to `EXPANDED_HUNK_CONTEXT` lines immediately after the hunk (starting at `hunk_last + 1`), clamped to `1..=total_lines`, skipping any line already covered by an earlier hunk/expanded block, and stopping expansion once `MAX_EXPANDED_CONTEXT_LINES_PER_FILE` expanded lines have been emitted for that file.
  - Runs `redact_secrets` once on the full `head_text` before extracting expanded-only lines (`:` lines, catching multi-line PEM private keys while preserving line numbers) while keeping all raw diff hunk lines (`+`, `-`, ` `) from `file.patch` **unredacted** (preserving [SPEC.md §8.3](../../SPEC.md#83-ephemeral-clone-read-only-tools--secret-redaction) hardcoded-secret detection).
  - Formats expanded lines as `{n:>5} : {code}` (distinguishable at a glance from `{n:>5}   {code}` diff context lines and `{n:>5} + {code}` added lines).

### 8.2 Workspace Loading & Batching Order (`tools.rs`, `prompt.rs`, `engine.rs`)

- `tools::load_expanded_diffs(ws: Arc<RepoWorkspace>, files: &[FileDiff]) -> HashMap<String, String>`:
  - Runs in `tokio::task::spawn_blocking`.
  - Skips `FileStatus::Added` (new files are already complete in their diff) and files whose `diff_block(file).chars().count() >= MAX_EXPAND_FILE_CHARS`.
  - Resolves each path through `ws.resolve` and reads via `read_text` (silently skipping secret paths, binary files, >2 MB files, missing files, or symlink escapes).
  - Stores `normalize_path(&file.path) -> expand_diff_block(file, &text)`.
- `prompt::batch_targets_with_blocks` / `DiffBatch`:
  - Update `batch_targets<'a>(targets: &'a [FileDiff], rules: &'a [Rule], max_chars: usize, expanded: &HashMap<String, String>) -> Vec<DiffBatch<'a>>` (or keep a small helper) so batch size estimation uses `expanded.get(&normalize_path(&file.path))` when present, else `diff_block(file)`.
  - `build_finder_user_prompt` takes `&HashMap<String, String>` (`expanded`) and renders each batch file via `expanded.get(&normalize_path(&file.path)).cloned().unwrap_or_else(|| diff_block(file))`.
- `engine.rs::review_targets`:
  - Moves `batch_targets(...)` to **after** `clone_pr` / `workspace` creation so `load_expanded_diffs` runs once when `workspace.is_some()`, and passes `&expanded` into `batch_targets` and `FinderInput`. Diff-only / Small PR paths pass an empty `&HashMap::new()`.
  - `allowed` (`CommentableRange`) continues to be computed strictly from raw Git patches (`full_targets` / `incremental_targets`), never from expanded `:` lines.

---

## 9. Kodus Reference (community code, behaviour only)

Read on 2026-11-24 at HEAD of `kodustech/kodus-ai`; nothing under `libs/ee/**` or `*.ee.*` was opened:

- `libs/agent-harness/README.md` — `AgentPolicy` + `Verifier<T>` separation (doer ≠ checker) and `RunState` observability.
- `libs/agent-harness/infrastructure/policies/completion-gate.policy.ts` — warns explicitly against the `#5 exhaustive trap` when steering agent stopping behavior.
- `libs/agent-harness/infrastructure/policies/budget.policy.ts` — cache-friendly step-budget pacing notes injected only at transitions.
- `libs/agent-harness/infrastructure/compression/context-compressor.ts` — shortening older tool results to a 400-char excerpt instead of blanking them, preventing redundant re-reads.
- `libs/code-review/pipeline/stages/build-orchestrator-input.ts` — threading PR context and repository instructions consistently across orchestrator inputs.

---

## 10. Assumptions

- `EXPANDED_HUNK_CONTEXT = 15`, `MAX_EXPAND_FILE_CHARS = 20_000`, `MAX_EXPANDED_CONTEXT_LINES_PER_FILE = 120`, and `anchor_retried` limit of 1 bounce per Finder batch agent are tuning values chosen for Adus from the `better-booru#12` trace and can be adjusted after live measurement.
- `better-booru#12` and `adus#2` PR metadata/comments inspected via public GitHub API (`manot40/adus#2`, `manot40/better-booru#11`, `manot40/better-booru#12`) reflect the live deployment behavior.

---

## 11. `SPEC.md` Sync

When implemented, update [docs/SPEC.md](../../SPEC.md):
- §8.3: note `load_expanded_diffs` secret redaction on `:` expanded lines while raw diffs remain unredacted.
- §8.4: document `Rules` specialist activation on `repo_instructions`, Finder anchor bounce (`validate_batch_anchors`) + `filtered_dropped` stage metric, `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000` / 400-char excerpt compaction, Verifier `<pr_title>`/`<pr_description>`/`<pr_manifest>` prefix + `<memories>`/PR-intent/hedge drop rules + `drop_suggestion`, and pre-seeded ±15-line hunk context.
- §10: add an entry for `2026-11-24-agentic-prompt-and-harness-design.md` and mark its status `implemented`.
