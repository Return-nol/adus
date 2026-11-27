# Review Engine Overhaul (Core Loop & Sharded Rules) — Design Specification

Date: 2026-11-25 · Status: approved

> **Binding scope & policy:** Extends [docs/SPEC.md](../../SPEC.md) §3.1, §6.1, §7, §8.3, and §8.4, and supersedes the removed `2026-11-24-finder-completion-gate-and-verifier-effort-design.md`. Follows the `AGENTS.md` Clean-Room and No-Assumption Rule: Kodus community code (outside `libs/ee/**` and `*.ee.*`) was consulted as a behaviour reference and is cited in §10; `libs/ee/**` and `*.ee.*` were never opened. All Rust code, prompts, and schemas are written fresh for Adus.

---

## 1. Intent & Why

Live debugging on `manot40/better-booru` PR #13 exposed three distinct failure modes in Adus's review harness compared with Kodus on the exact same OpenRouter model:

1. **Shallow / zero-tool Finder exits (Sonnet):** Finished in 9 seconds, 1 step, 0 tool calls, 0 findings. Pre-expanded 15-line `:` hunk context, a 4,000-character PR description, unlocked `submit_findings` on Step 1, and early-exit wording (*"Do not keep searching just because steps remain"*) let the model treat the PR as trivial without opening any file.
2. **Single-pass anchoring & under-powered Verification (DeepSeek):** Took 4m 28s, found the first two issues (`url_builder.go`, migration leak), missed `cache.go:124-137` (swallowed `s3Storage.Exists` error), and had its `url_builder.go:46` finding refuted when the `Low`-effort 6-step `fast_model` Verifier hit its step cap mid-investigation and guessed.
3. **Agentic Rules under-coverage:** Adus dumps team `<rules>` into `Generalist` or a single agentic `Rules` specialist that can skip files, rather than deterministically sweeping every rule-applicable file and verifying rule citations against real source excerpts.

**Success** (measured by the §9.2 acceptance run on PR #13): The Finder investigates all changed hunks with `read_file`, runs a second Synthesis Rescue pass for missed bugs, verifies findings on `primary` at `High` effort with auto-widened reads and an Evidence Gate, adds a title and, where the fix is clear, a checked suggestion to each surviving finding (never rewriting its verified body), and evaluates team `<rules>` with a parallel per-file Sharded Judge + Evidence Collector/Judge — all without AST call-graph generation, container sandboxing, database migrations, or UI changes.

---

## 2. Goals, Non-Goals & Hard Constraints

### Hard Constraints (Non-Negotiable)

1. **Lightweight architecture:** No AST call-graph parser (no `tree-sitter`) and no container sandbox. Keep Adus's shallow git clone (`RepoWorkspace`/`RepoPath`) and in-process `Toolbox` (`read_file`, `grep`, `list_dir`, `find_files`).
2. **Two model slots (`primary` + `fast`) with raised minimum efforts (Option 1):**
   - `primary` at `ReasoningEffort::High`: Finder Pass 1 (`Generalist`, `Bug`, `Security`, `Performance`) and Finder Pass 2 (`synthesis-rescue`).
   - `primary` at `ReasoningEffort::High`: Surgical Verifier (7-step light pass + 10-step Evidence Gate re-verify). It verifies at the Finder's own effort, as Kodus does (its verifier reuses the finder's model and effort): a weaker verifier refutes real findings on thin evidence, which is what dropped the DeepSeek `url_builder.go` finding.
   - `fast` at `ReasoningEffort::High`: Sharded Rule Judge, Rule Evidence Collector (6 steps), Rule Evidence Judge (user decision 2026-11-26; Kodus runs the shard on its review model and the collector and judge without reasoning — Adus keeps all three on the cheaper slot and buys depth with effort instead).
   - `fast` at `ReasoningEffort::Medium`: Severity Classifier, Post-Verify Enricher, Semantic Dedup, and Prose Recovery.
   *(Note: `learn_pr` extraction in `crates/review/src/learn.rs` is outside the review pipeline and remains unchanged.)*
3. **Zero DB migrations, API changes, or UI changes:** Keep the existing `Rule` schema (`path_glob: Option<String>`, `severity`, `enabled`) and 3-level `Severity` (`info`, `warning`, `critical`). All verified findings are posted (no severity drop filter).

### Non-Goals

- AST call graphs, `checkTypes`, `ast-grep`, or container syntax validation of suggested patches.
- Hardcoded model-name profile tables for Synthesis Rescue (Pass 2 runs for all models when time and budget allow).
- Separate `scope: pull_request` DB column or rule `detector` / `examples` schema changes. Known loss: without a PR scope, every rule is judged one file at a time, so a rule about the PR title or description can never be confirmed (no file excerpt shows it) and a cross-file rule ("a migration needs a matching down file") is judged per file. Kodus sends such rules to one whole-PR shard.
- Any changes to `SPECIALISTS_REVIEW_TIMEOUT` (30 min), `JOB_TIMEOUT` (31 min), `small_pr` fast-path eligibility, or `web/`.

---

## 3. OpenRouter Reasoning Wiring & Live Tracing (`crates/llm`, `crates/review`)

1. **Live Tracing Cleanup:** Commit `ChatResponse::reasoning()` (`crates/llm/src/types.rs`), `adus_review::trace` prompt/response/result tracing (`agent.rs`), and `finder`/`verify` debug spans (`finder.rs`, `verifier.rs`).
   - Keep `"display": "summarized"` on adaptive-thinking Claude requests (`crates/llm/src/anthropic.rs`) as permanent behaviour, not an experiment: it changes only what the response shows (thinking is billed the same), and without it Claude 4.7+ / 5.x models return empty thinking blocks, so the trace shows no reasoning.
   - Remove the other `TEMP(experiment)` overrides: the Generalist Finder effort is set by §2 (`High`), and the Workflow wording by §4.4.
   - Keep the hidden `adus rereview <repository_id> <pr_number>` command (and its `uuid` dependency) as a documented debug-only command: it queues `Job::ReviewPr` with `Trigger::Opened` and is how the §9.2 acceptance run is repeated. Replace its `TEMP(debug)` comments with a doc comment stating it is hidden and debug-only.
2. **OpenRouter Detection:** In `OpenAiClient` (`crates/llm/src/openai.rs`), detect OpenRouter when `base_url` parses with host `openrouter.ai` or `*.openrouter.ai` (case-insensitive, e.g. `https://openrouter.ai/api/v1`).
3. **Why this changes SPEC §6.1 (2026-11-24 reasoning-effort spec):** OpenRouter accepts bare `reasoning_effort`, but by default it may route a request to an upstream provider that does not support every parameter sent and silently drops the unsupported ones, so a Finder call can run without reasoning (or without tools) and nothing reports it. `provider.require_parameters: true` restricts routing to upstreams that honour every parameter; `reasoning: { effort }` is OpenRouter's documented unified reasoning parameter. This supersedes the OpenRouter part of §6.1; direct DeepSeek and other OpenAI-compatible endpoints keep `reasoning_effort`.
4. **OpenRouter Wire Payload & Fallback:**
   - When `is_openrouter` and `effort: Some(e)`: send `"reasoning": { "effort": e.as_str() }` and `"provider": { "require_parameters": true }` instead of bare `"reasoning_effort"`.
   - If OpenRouter responds with an error (`400` or `404` / `LlmError::Invalid`) whose lowercase message contains `"no endpoints found"` (a non-reasoning model rejecting `require_parameters: true`), retry once without `"provider": { "require_parameters": true }` before falling back to `effort = None` via `chat_with_effort_fallback`. This retry is Adus's own design: Kodus avoids the error by sending `require_parameters` only to models its capability table marks as reasoning by default, and Adus has no such table.

---

## 4. Eager Core Finder Loop (`crates/review`)

### 4.1 Raw Diffs & PR Context Cap

- Remove `expand_diff_block` (`prompt.rs`) and `load_expanded_diffs` (`tools.rs`, `engine.rs`). Tool-enabled Finder runs always receive raw unified hunks (`diff_block`) so the model must open files with `read_file` to read surrounding code.
- Cap `<pr_description>` at `MAX_DESCRIPTION_CHARS_TOOLS = 500` when `with_tools == true`, keeping `MAX_DESCRIPTION_CHARS_DIFF_ONLY = 4_000` for diff-only runs.
- **`<changed_functions>` (no AST):** for tool-enabled runs, list per file the enclosing-scope names git writes after each hunk header (`@@ -a,b +c,d @@ <context>`, e.g. `func GetCache(ctx context.Context, …`), trimmed to the text before the first `(` or `{` when one exists, deduplicated per file in hunk order, at most `CHANGED_FUNCTIONS_MAX = 30` entries in total; omit the section when no hunk header carries context. It gives the model the to-do list that Kodus's call graph provides; the Workflow (§4.4) tells it to grep the callers of each entry. Headers come from the provider's patch text, so the list is a hint and may be empty for some languages or providers.

### 4.2 Per-Hunk `CoverageLedger` & One-Shot Gate (`coverage.rs`, `agent.rs`, `tools.rs`)

- **Target tracking:** `CoverageLedger::from_files(files)` builds one target per file in the batch using `adus_git::diff::commentable_hunks(patch)` to extract right-side `[start, end]` line ranges (files with only deletions or empty hunks have an empty range list and mark covered on any successful `read_file`).
- **What counts as read:** Only **successful (`!result.is_error`)** `read_file` calls executed in **earlier** turns update `CoverageLedger::record_read(rel_path, start, end)`.
  - `Toolbox::read_file_range(&self, call: &ToolCall) -> Option<(String, u32, u32)>` extracts `(normalized_rel_path, start_line, end_line)` for `read_file` calls using `RepoWorkspace::resolve_existing_file`. When `end_line` is omitted or read output is capped at `READ_MAX_LINES = 200` / `READ_MAX_CHARS = 8_000`, `record_read` uses the actual `[shown_start, shown_end]` parsed from the `[path: lines X-Y of Z]` header (or `0-0` for an empty file, covering all hunks).
  - `grep`, `list_dir`, `find_files`, failed `read_file` calls (`PastEnd`, `BadRange`, `NotFound`, etc.), and `read_file` calls issued in the same turn as `submit_findings` do **not** count.
- **Which runs get a ledger:** only the gated Finder runs — `Generalist` and `Bug` Pass 1 with `AgentTools::Full` — get a `CoverageLedger`, and so debt notes and the gate. `Security` and `Performance` look through a narrow lens and may rightly ignore a file, so they get neither; nor do Pass 2 (§4.5), the Verifier, or rule-evidence runs.
- **Debt targets:** only qualifying files carry debt: not a doc file (`GATE_DOC_EXTENSIONS = ["md", "mdx", "rst", "txt"]`) and not a test path (`*_test.*`, `*.test.*`, `*.spec.*`, `tests/**`, `__tests__/**`). The note and the gate below use the same set, so the model is never asked to read a file the gate would not require.
- **Debt note (`prepare_step`), append-only:** Before a step when `with_repo_tools == true`, `!wrap_up.active()`, at least one qualifying file has pending hunks, **and the pending set differs from the one in the last debt note sent in this run** (the first step after the initial prompt always counts as different), append the note to the conversation and keep it there. Never inject a message for one request only and drop it from the next: the history stays append-only, which keeps the prompt-cache prefix valid and avoids the history-editing check of Claude models with preserved thinking (Sonnet 5.5, Opus 5.5), where removing an earlier message invalidates the thinking blocks that follow it. The note is a user text turn placed after the step's tool results; the provider wire mapping keeps roles alternating (on Anthropic it becomes a trailing text block of the same user message that carries the `tool_result` blocks). It lists up to `COVERAGE_DEBT_MAX_FILES = 8` files and up to `COVERAGE_DEBT_MAX_RANGES = 3` pending ranges per file:
  ```text
  Unread changed code. You have not opened these changed lines with read_file yet:
  - path/to/file.rs: lines 40-55, 80-95
  Read them before you call {submit}. Only read_file opens code; search and directory results show where it is, not what it does.
  ```
  (Wording written for Adus; it must not reproduce Kodus's debt-note text.)
- **Deliberate differences from Kodus (recorded 2026-11-26):** Kodus sends its debt note with every request for that request only and tells the model it "does not need to read every hunk" (depth over breadth). Adus keeps the note in the conversation (cache and preserved-thinking reasons above) and asks for every qualifying hunk to be read, because the PR #13 runs showed models answering from the diff alone.
- **One-shot hard refusal gate (`CompletionGate`):** Enabled for `Generalist` and `Bug` with `AgentTools::Full` (`Security`, `Performance`, and diff-only are not gated). On the first parsed submission (`submit_findings` tool call OR prose JSON) when **all** hold:
  1. The gate has not refused before in this run (`!gate_refused`);
  2. `!wrap_up.active()`;
  3. `max_steps.saturating_sub(wrap_up_steps.max(1)).saturating_sub(steps_used_including_current) >= GATE_MIN_STEPS_LEFT (2)`;
  4. At least one qualifying file (see Debt targets) still has pending hunks.
  Refuse ONCE with `ToolResult { is_error: false, .. }` (or a `Message::User` for prose submissions):
  ```text
  Not submitted yet. Before you submit, open these changed files with read_file and check the code around each change: {files}. Then call {submit} again with every finding you confirmed. This is asked only once; your next {submit} call will be accepted.
  ```
  Hold that first submission: if the run later ends unparseable, return the held submission with `cut_short: true, gate_refused: true`. Block any second `submit_findings` call in the same refusal turn.
- **Empty resubmission keeps earlier findings:** when the accepted submission has no findings but an earlier submission of the same run (the held one, or any refused one) had findings, the run's result uses the most recent earlier non-empty findings (the accepted submission's `review_log` is kept). A model refused once often resubmits `[]` meaning "already sent"; Kodus measured this losing findings and falls back the same way.
- **Why a one-shot refusal and not Kodus's repeated veto:** Kodus vetoes every submit, but only while critical-tier files are unread, and gives files tiers only when a PR is over its prompt budget. It turned the gate off for PRs that fit because the veto "can run a finder that answers from the diff to its step cap". Adus refuses at most once on every PR: enough to make a model that skipped the files open them, never enough to trap it. The §9.2 acceptance run checks that no Finder run ends at its step cap because of the gate (`finder_steps < max_steps` or `gate_refusals == 0` for runs that hit the cap).

### 4.3 Budget Policies, Step Caps, Compaction & Timeouts

- **Adaptive Finder steps (`finder_max_steps`):** For a batch of `files` files:
  - `FINDER_BASE_STEPS = 20`, `FINDER_FREE_FILES = 8`, `FINDER_STEP_CAP = 30`.
  - `finder_max_steps(files) = (20 + files.saturating_sub(8).div_ceil(2)).min(30)`.
  - `FINDER_WRAP_UP_STEPS = 2`.
- **Escalating `BudgetPolicy` bands (`agent.rs`), Finder only:** enabled by `AgentConfig::budget_bands = true`, which only Finder passes set (Verifier, rule evidence collector and other agent runs leave it `false`; their texts mention findings and dismissals, which mean nothing to a verifier). When enabled, `with_repo_tools == true`, `!wrap_up.active()`, and `max_steps >= 6`:
  - `force_start = max_steps.saturating_sub(wrap_up_steps.max(1))`
  - `urgent_from = force_start.saturating_sub(3).max(3)`
  - `encourage_from = urgent_from.saturating_sub(4).max(2)`
  - Emit each band's note **once** when transitioning into `Encourage` or `Urgent` (stored in conversation so the prefix remains cacheable). Remove the old `steps_used == max_steps / 2` note, whose "call submit as soon as … do not spend extra steps" wording pushed DeepSeek to stop early.
  - Note texts (Finder; `{submit}` is the run's submit tool):
    - `Encourage`: `Step budget: {used} of {max} steps used. Keep going through the changed code you have not checked yet, and batch independent lookups in one turn.`
    - `Urgent`: `Step budget: {left} steps left before only {submit} is offered. Finish the check you are on, then call {submit} with every finding and dismissal so far.`
  - Neither text tells the model to stop early or to save steps; the final wrap-up steps (`FINDER_WRAP_UP_STEPS = 2`) keep the existing "submit now" message.
- **Compaction (`agent.rs`):**
  - `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000`, `COMPACT_EXCERPT_CHARS = 400`, `COMPACTED_MARKER = "\n[older tool output shortened to save context]"`.
  - Shorten oldest `ToolResult` messages outside the first prompt and `COMPACT_KEEP_TAIL = 4` only when length `> 446` and not already ending with `COMPACTED_MARKER`.
- **Cache-weighted Token Budget (`budget.rs`):**
  - `CACHE_READ_DIVISOR = 4`: `TokenBudget::add(input_tokens, cache_read_tokens)` adds `uncached + cached.div_ceil(4)` where `cached = cache_read_tokens.min(input_tokens)` and `uncached = input_tokens - cached`. Cache writes count in full.
- **Token budget estimate (no default change):** `max_review_input_tokens` keeps its `2_000_000` default (changing it needs a migration, ruled out by §2). Rough estimate for an 8-file Generalist PR with 6 candidates, raw input tokens: Pass 1 ≈ 300–500 K, Pass 2 ≈ 200–400 K, light verify ≈ 6 × 7 steps × ~20 K ≈ 840 K, Evidence Gate ≈ 0–400 K, post-verify funnel ≈ 50 K, rules shards and evidence ≈ 100–300 K when rules exist: ≈ 1.5–2.4 M raw. Most of it is cache reads on providers that cache (a growing agent conversation re-sends its prefix), so the weighted total (cache reads at ¼) is expected to be ≈ 0.6–1.2 M. These are estimates; the §9.2 acceptance run records `budget_used` and the raw input total, and the default is revisited in a follow-up (with its own migration) only if real runs skip Pass 2 or cut verification for budget.
- **Token budget estimate, `Specialists` mode (same 8-file PR, one batch, ~10 candidates):** `Bug` Pass 1 ≈ 300–500 K; `Security` and `Performance` Pass 1 ≈ 150–350 K each (no debt notes or gate, so usually fewer steps); `Bug` Pass 2 ≈ 200–400 K; light verify ≈ 10 × 7 × ~20 K ≈ 1.4 M; Evidence Gate ≈ 0–600 K; funnel ≈ 50 K; rules ≈ 100–300 K: ≈ 2.4–4.0 M raw, ≈ 1.0–2.0 M weighted. That is close to the 2 M default, so Specialists reviews of larger PRs are the likeliest to skip `Bug`'s Pass 2 (`SYNTHESIS_MIN_BUDGET_SHARE`) or cut verification. Operators can raise `max_review_input_tokens` in Settings without a code change; the §9.2 Specialists run measures how close it comes.
- **Timeouts (`engine.rs`, `crates/adus/src/worker.rs`):**
  - Raise `REVIEW_TIMEOUT` from `12 min` to **`20 min`** (`Duration::from_secs(20 * 60)`), preserving `REVIEW_TIMEOUT < SPECIALISTS_REVIEW_TIMEOUT (30 min) < JOB_TIMEOUT (31 min) < LOCK_TTL / RECLAIM_IDLE / STALE_AFTER (35 min)`.
- **Stage deadlines (`engine.rs` `Deadlines`):** the post-verify funnel now makes LLM calls (severity, per-finding enrichment), so the fractions of the run's budget move from Finder 60 % / Verifier 90 % / Dedup 97 % to:
  - `finder` 55 % (Pass 1 + Pass 2 + sharded rules, clone included): 11 min of 20, 16.5 min of 30;
  - `verifier` 80 %: 5 min of 20;
  - `post_verify` 93 % (renamed from `dedup`; Semantic Dedup, Severity and Enrichment all finish by it): 2.6 min of 20;
  - the remaining 7 % (84 s of 20 min) is for `check_fix`, comment formatting and posting.
  - Enrichment stops starting new calls when less than `ENRICH_RESERVE = 20 s` is left to `post_verify`; findings not enriched by then keep the fallback title (§6.3).

### 4.4 Adversarial Multi-Lens Prompt & Lean Finder Schema (`prompt.rs`, `specialists.rs`, `parse.rs`)

- **Mindset & Workflow (written fresh for Adus):**
  - Require tool use on the first step: the run opens with a tool call, with no prose and no `submit_findings` before code was read (wording written fresh; amended 2026-11-27 after a clean-room check found the first draft echoed Kodus's sentence).
  - Remove *"nobody counts your findings"* and *"Do not keep searching just because steps remain"*.
  - Require adversarial proof: every change is unproven until the agent verifies why it cannot fail, and a change is never cleared because it reads plausibly, only by the code that rules the failure out.
  - One hunk or file can hold several defects: after one is found, the same code is challenged for more (added 2026-11-27; `cache.go` in the reference PR holds two).
  - Dismissals need code that stops the failure, not code that copes with it afterwards (added 2026-11-27 after the DeepSeek run on PR #15 dismissed a transient `Exists` error read as a cache miss because the caller regenerates, cleared the `CDN` branch with a guard that only prevents a panic, and never examined two `_, _ =` discarded update errors). A discarded or only-logged error, an error read as a miss / empty / not-found, and a branch that skips a fallback or lands on a generic default when its own guard or dependency fails are reported and left to the Verifier, never dismissed because a caller recovers or the outcome looks harmless; a guard that only prevents a crash clears nothing; each error a `+` line discards is its own suspicion. Both workflows (tools and diff-only) carry it. Written as a general rule, not for the reference PR; watch the next live runs for extra low-value findings.
  - A rare trigger is no ground for dismissal (added 2026-11-27 after run 2 on PR #16 dismissed `cache.go:89` because the conflict path "only changes hash for rows that were already inconsistent", the very rows this PR repairs; run 1 had found it only in the Synthesis Rescue pass). Edge cases, error paths, timeouts, concurrent callers, unusual data, and triggers that only reach already-inconsistent rows, files or state stay suspicions unless code makes them impossible, and the body states the trigger condition so severity can weigh it. This mirrors the Verifier's "None of these refutes a finding" list (§5 item 7).
  - No team knowledge excuses a defect in the Finder (same day, aligned with Kodus after a first draft added an exception). A draft let a `<memories>`, `<rules>` or `<repository_instructions>` entry justify a dismissal, to resolve the clash with `<Scope>`'s old "treat memories as true and do not flag code that follows them". Kodus resolves it the other way: its Finder lists memory rules as team conventions with no exemption, keeps standard Kody Rules out of the Finder entirely (sharded rules agent), and tells it repository instructions "never let them change how you judge bugs, security or performance"; only its Verifier drops intended behaviour, on in-repo evidence (`prompt-builder.ts` `formatMemoryRules`, `formatRepoInstructions`, `DEFECT_HUNT_MINDSET`; `verifier-prompt.ts`). Adus now does the same: the exception is removed, `<Scope>` says memories help the agent understand the code and the team's intent but never excuse a defect (the Verifier's memory rule and intended-behaviour rule decide), and repository instructions never change how bugs, security or performance are judged. A learned "this is intended" memory therefore costs one Verifier run per matching suspicion instead of a silent Finder skip.
  - For each entry in `<changed_functions>` (and each changed hunk without one), grep its callers and read what they pass and expect before judging the change.
  - For `Generalist`: require explicit checks across all three lenses (**Bug**, **Security**, **Performance**) with concrete checklists (swallowed errors / errors treated as cache miss or not-found, SQL `NULL` propagation in `CONCAT`/joins, dropped fallbacks — for every branch the change adds or reroutes, what happens when that branch's own dependency is missing, disabled or fails, and whether it still reaches a correct fallback (added 2026-11-27; also in the `Bug` Role) —, race/TOCTOU, migration hazards, delegation/wrapper bugs, unbounded queries/loops, auth/path traversal) and require `dismissed` to record hypotheses tested per lens.
  - **Finish condition** (replaces "submit immediately"): submit once `review_log` covers every changed function and every lens. No sentence in the Workflow, Role or budget notes tells the model to submit early or to avoid spending steps.
- **Lean Finder Schema (`submit_findings`):**
  - `review_log` (**required** string, listed before `findings` in the schema and the prompt; named `reasoning` until 2026-11-27): a record of the review work on each changed function or hunk: the lenses applied, the callers, callees and files read, and each suspicion with its outcome (reported, or dismissed with the code that rules it out). Renamed and reworded because Claude Sonnet 5.5 declined every Finder request with `stop_reason: "refusal"`, category `reasoning_extraction` (Anthropic's classifier for requests that ask the model to write out its reasoning; server-side fallback never retries it). Kodus's `submitResult` keeps `reasoning` with "why you reported or dismissed"; Adus departs from it here. No Finder prompt or schema contains the word "reasoning" (prompt test), and a submission that still sends `reasoning` is read the same way. Models write it before the findings, so it is where the investigation is accounted for; an optional summary is skipped in practice. The parser accepts a submission without it (a missing or empty `review_log` is logged as `finder review log missing` at debug, never refused), so a non-compliant model loses no findings. It is logged at debug and never posted.
  - `findings` array of:
    - `file`: string (required)
    - `start_line`: integer (optional)
    - `line`: integer (required, `+` line in diff)
    - `category`: `"bug" | "security" | "performance" | "repo_instruction"` (required when `Generalist`; inferred from `SpecialistKind` otherwise, accepted either way)
    - `body`: string (required — concrete trigger, impact, and code path; this text is posted as written after verification, §6.3, so the Writing rules of the prompt apply to it)
  - Keep optional `dismissed` (`file`, `line?`, `suspicion`, `why_safe`).
  - `title`, `severity`, and `suggestion` are **no longer requested from the Finder** (leaving `Suggestion.title`, `Suggestion.severity`, and `Suggestion.suggestion` populated by the Post-Verify Funnel in §6; diff-only/legacy JSON parsers remain backwards-compatible).
  - **Stringified arrays:** when `findings` (or `dismissed`) arrives as a JSON-encoded string instead of an array, parse the string and use it if it is a JSON array; any other string is rejected as before. Claude models (Sonnet 5.5 measured by Kodus) often send the array this way inside an otherwise valid tool call, and without this every finding in that call is lost.

### 4.5 Prose Recovery & Pass 2 Synthesis Rescue (`finder.rs`, `parse.rs`)

- **Prose Recovery (`recover_prose_findings`):** After Pass 1 or Pass 2, if structured `findings` is empty and the agent's final `text` (or `review_log` if available) is `>= 80` chars and matches at least 2 of 3 signal groups (defect words, action words, file/line references), call `fast` at `Medium` once (`MAX_TOKENS = 2_048`) to extract structured lean findings anchored strictly to files in the batch.
- **Pass 2 Synthesis Rescue (`synthesis-rescue`):**
  - Runs for **all models** after Pass 1 on tool-enabled `Generalist` and `Bug` agents when:
    1. The token budget has at least `SYNTHESIS_MIN_BUDGET_SHARE = 25 %` of `max_review_input_tokens` left (weighted, §4.3), so Pass 2 does not leave the Verifier without budget;
    2. Remaining time to `deadlines.finder` is at least `SYNTHESIS_MIN_WINDOW = max(6 × pass1_mean_step, 60 s)`, where `pass1_mean_step` is the mean wall time of Pass 1's LLM steps (90 s when Pass 1 measured none). Half of `SYNTHESIS_MAX_STEPS` at Pass 1's own pace is the least room in which a pass can read a few files and submit; the run's normal deadline wrap-up still forces a submission if Pass 2 runs long.
  - When either condition fails, Pass 2 is skipped and the stage log records `synthesis_skipped = "budget" | "time"`.
  - Reuses the exact same `Toolbox` (shared `ToolCache`) and `finder_max_steps(files).min(SYNTHESIS_MAX_STEPS (12))`, with **no** `CompletionGate` refusal (Pass 1 already read the files) and **no** coverage-debt spam.
  - Sends the same base prompt prefix plus:
    - `<already_inspected_files>` (sorted list of files opened with `read_file` in Pass 1)
    - `<recent_investigation>` (last 20 tool calls from Pass 1, secret-redacted, args capped at 150 chars)
    - `<current_findings>` (Pass 1 findings: `- file:line: body_excerpt`)
    - Task: re-think the review for concrete **missed** defects (especially error paths, callers, edge cases, untouched branches) without repeating or varying Pass 1 findings; submit `[]` if none.
  - Merges additions by `(normalized_file, line, normalized_body_prefix)` and unions `read_files` across Pass 1 + Pass 2 into `CandidateFinding.investigated_by_finder: bool`.
  - Every candidate carries `origin: FinderPass` (`Pass1` | `SynthesisRescue` | `ProseRecovery`), logged on the Verifier's per-candidate verdict debug line and counted per origin in the stage log (`kept_by_origin`), so a posted comment can be traced to the pass that produced it.

---

## 5. Surgical Verifier + Evidence Gate (`verifier.rs`, `tools.rs`)

1. **Model & Effort:** Run on **`primary_model` at `ReasoningEffort::High`** (`VERIFY_EFFORT = ReasoningEffort::High`), the Finder's own effort (§2).
2. **Step Caps & Final Step Guarantee:**
   - `VERIFY_LIGHT_MAX_STEPS = 7`.
   - `VERIFY_FULL_MAX_STEPS = 10`.
   - Set `no_tools_on_last_step: true` in `AgentConfig`: on the final step (`steps_used + 1 == max_steps`), offer **no tools** (strip `read_file`/`grep`/`list_dir`/`find_files` AND `submit_verdict`) and instruct the model to respond with ONLY the final JSON verdict (`{"keep": true|false, "rationale": "..."}`). Earlier steps still offer read-only tools + `submit_verdict`.
   - **Verdict-due note:** with `no_tools_on_last_step`, append once (append-only, like the debt note) before step `max_steps - 1` (two steps before the end, so the model has one tool step left): `One step left for reading. After it you must answer with your final JSON verdict, without tools, based on what you have read.` The Finder's budget bands are not used here (§4.3).
3. **Auto-Widening `read_file` in Verifier:** When `AgentConfig::widen_short_reads == true` (enabled for Verifier), any `read_file` call with explicit `start_line` and `end_line` where `end_line >= start_line` and `end_line - start_line < 20` is widened to `start_line.saturating_sub(20).max(1)` .. `end_line.saturating_add(10)` before hitting `Toolbox` (still bounded by `READ_MAX_LINES = 200` and `READ_MAX_CHARS = 8_000`), ensuring enclosing guards and error handlers are visible.
4. **Evidence Gate:** After the 7-step light pass, filter kept findings where `!candidate.investigated_by_finder` (the Finder never completed a successful `read_file` on `candidate.suggestion.file` in Pass 1 or Pass 2). Re-verify those findings in a second pass with `VERIFY_FULL_MAX_STEPS = 10`, sharing `Toolbox`/`ToolCache`. The Evidence Gate verdict supersedes the light verdict when the gate run produces one. A gate run that ends without a verdict (deadline cut, provider error, no final reply) keeps the light verdict: failing to state a refutation is not one, and gate runs start last, so the deadline would otherwise cut exactly the findings the gate exists to strengthen. Only findings whose light run used tools go to the gate; a light verdict that came from a single diff-only call (budget exhausted, or tools rejected) is final, and a gate run is never started once the token budget is exhausted, since it could only repeat that call (user-delegated controller ruling 2026-11-26).
5. **Verdict Semantics:** Keep-by-default (`keep: true`) when evidence cannot refute.
   - **Dropped as unverified** (`dropped_unverified`): a provider error, a deadline cut, or a run that ends without ever producing a reply to parse (aborted or cut short before its final step).
   - **Kept when the verdict cannot be read:** a run that reaches its final reply but whose verdict cannot be parsed is first recovered — from a `submit_verdict` call with loose arguments, then from a JSON object (or a bare `"keep": true|false`) in the reply text. If nothing usable is found, the finding is **kept** (logged `verifier verdict unreadable, kept`). The Verifier exists to refute; failing to state a refutation is not one. Kodus keeps such findings the same way.
6. **Candidate Queue (no count cap):**
   - **`MAX_VERIFY_CANDIDATES` is removed**, with `VerificationReport.truncated_by_cap` and the engine warning `"… exceeded the verification cap and were omitted"`. Every candidate with `needs_verify` is queued. The old cap sorted by `severity`, which no longer exists at verify time (§4.4, §6.2), so it would have kept the first 25 in Finder order and dropped the rescue pass's findings first. Kodus has no count cap either: it verifies every candidate, four at a time, bounded only by its per-agent timeout. Adus stays bounded by the Verifier deadline (§4.3) and the review's `TokenBudget`; a candidate that starts after the deadline is dropped as unverified, and one that starts after the budget runs out gets the existing single diff-only call.
   - **Queue order** (`verify_order` in `verifier.rs`): because the deadline and the budget cut the end of the queue, candidates start in a fixed order so the least important are the ones cut. A stable sort by:
     1. **Category:** `security` → `bug` → `performance` → `repo_instruction`. A candidate without a `category` takes it from its `SpecialistKind` (`Security` → security, `Performance` → performance, `Bug` and `Generalist` → bug).
     2. **Origin:** `Pass1` → `ProseRecovery` → `SynthesisRescue` (§4.5).
     3. **Original position** (the sort is stable).
   - The Evidence Gate's re-verify (item 4) queues its findings in the same order.
   - `VERIFY_CONCURRENCY` stays 4. `verify_candidates` still returns kept findings in their original order, and the existing `"{n} candidate finding(s) could not be verified and were omitted"` warning still reports deadline cuts.
7. **Refutation rules (amended 2026-11-27 after the Kodus comparison):** never grounds to drop — a rare trigger (edge case, error path, timeout, concurrent callers, hostile input); damage that surfaces in another function or file when the changed line sets it off (the Verifier follows the path first; the 2026-11-24 spec already adopted this clause but it had not reached the prompt); older code the change makes reachable or worse; an expectation that a library, the runtime or timing prevents the failure that nothing read confirms. The hedge rule drops only bodies that ask for a check and state no failure; a body that states a concrete failure and admits which premise is unconfirmed is a claim to check, because the Finder is told to say which part is unproven (§4.4). The intended-behaviour rule is reworded fresh.
8. **Verdict shape and logs:** `submit_verdict` is `{keep, rationale}`. `drop_suggestion` is removed: the Finder no longer sends suggestions (§4.4) and Enrichment writes them after verification, gated by `check_fix` (§6), so the Verifier never sees one. The rationale (secret-redacted, ≤ 300 chars) is logged on the per-candidate verdict debug line, and `VerificationReport.kept_unreadable` (stage log `verifier_kept_unreadable`) counts findings kept only because the verdict could not be read.

---

## 6. Post-Verify Funnel (`dedup.rs`, `severity.rs`, `enrich.rs`, `engine.rs`)

Stage order for surviving findings after Verification:

1. **Semantic Dedup (`dedup.rs`):** Runs on `fast` at `Medium` (`DEDUP_MAX_TOKENS = 1_024`). Merges duplicate root causes across Finder passes/specialists and drops findings already raised in existing PR threads (`threads_for_dedup`). Two findings are duplicates only when one fix would resolve both; different calls, ignored errors or branches stay separate even in one function, and when unsure both are kept (added 2026-11-27: the reference `cache.go` holds two distinct error-handling defects). Every removed finding is logged at debug with file, line, origin and, for `already_raised`, the cited thread's path, line and URL.
2. **Severity Classification (`severity.rs`):** Runs on `fast` at `Medium` (`SEVERITY_MAX_TOKENS = 1_024`) for all findings with `severity.is_none()` (findings that already carry an explicit severity, such as Sharded Rule findings, keep it untouched). Maps each finding to Adus's `Severity` (`info`, `warning`, `critical`); falls back to `Severity::Warning` on LLM/parse failure (never drops findings).
3. **Post-Verify Enrichment (`enrich.rs`):** Runs concurrently (`ENRICH_CONCURRENCY = 4`, bounded by `deadlines.post_verify` minus `ENRICH_RESERVE`, §4.3) on `fast` at `Medium` (`ENRICH_MAX_TOKENS = 2_048`) for each surviving finding:
   - Input: the finding (`file`, `start_line`, `line`, `category`, `body`), its file's diff hunk, and the surrounding head-file lines (`read_file_snippet`).
   - Output JSON: `{ "title": "<concise 1-line summary>", "start_line": <optional u32>, "suggestion": "<optional exact replacement code>" }`.
   - **The verified `body` is never rewritten.** The Verifier checked the Finder's `body`; a rewrite after verification could add claims nobody checked. Enrichment only adds a title and, when the fix is clear from the code shown, a suggestion. The prompt says the title must restate the body's problem and add nothing the body does not say. The posted comment body is the Finder's `body` (secret-redacted as today).
   - On failure: derives `title` from the body's first sentence (capped at 100 chars) with `suggestion: None`.
4. **Deterministic Fix Gate (`check_fix` in `enrich.rs`):** Before formatting inline comments, validate any `suggestion`:
   - Strip `suggestion` and `start_line` (downgrading to a plain comment — never dropping the finding!) if: empty/whitespace-only, identical (ignoring trailing whitespace) to the existing lines it replaces, outside the file's `CommentableRange` hunk (`!spans_one_hunk`), contains markdown fence markers (`` ``` ``), or its net `()`, `[]`, `{}` balance (outside string and char literals) differs from that of the lines it replaces (it must be zero when the replaced lines are unknown). Comparing with the replaced lines keeps a fix of a line that opens or closes a block.

---

## 7. Deterministic Sharded Team Rules + Evidence Loop (`rules_judge.rs`, `rules_evidence.rs`)

Replaces `SpecialistKind::Rules` and stops putting `<rules>` into tool-enabled `Generalist` (diff-only / `small_pr` / no-clone fallback keeps `<rules>` in the Generalist prompt).

**Repository instructions keep a home.** `SpecialistKind::Rules` checked team rules **and** repository instructions (`AGENTS.md` and similar). Sharded Rules replaces only the team-rules half. In `Specialists` mode the `Bug` specialist takes over repository instructions: its user prompt carries `<repository_instructions>`, its Role tells it to check the changed hunks against them, and it may report findings with `category: "repo_instruction"`. `Generalist` already carries `<repository_instructions>` and the `repo_instruction` category (§4.4), so nothing changes there. `Security` and `Performance` keep `<repository_instructions>` as context only.

1. **Parallel Execution:** When `toolbox.is_some()` and `!rules.is_empty()`, `engine.rs` runs `run_sharded_rules(...)` concurrently with `run_finder(...)` under `deadlines.finder`, sharing `Toolbox` and `TokenBudget`.
2. **Per-File Shard Sweep (`rules_judge.rs`):**
   - For each changed file in `files` with at least one matching enabled `Rule` (`rule_matches_path`), build one shard prompt containing:
     - Numbered matching rules `[1]..[n]` (title, severity, body)
     - The file's annotated diff (`diff_block`)
     - For non-`Added` files within `RULE_FILE_MAX_LINES = 600` and `RULE_FILE_MAX_CHARS = 30_000`, the full redacted head file content (`Toolbox::read_head_file_capped`)
   - Dispatch shards with `SHARD_CONCURRENCY = 4` on `fast` at `High` (`SHARD_MAX_TOKENS = 2_048`, plus the client's `High` reasoning reserve).
   - Lenient wire parser accepts `{"violations": [...]}` or bare `[...]`, coerces stringified line numbers, and ignores out-of-range `rule_id`s.
   - **Anchoring by moving to the nearest `+` line:** a violation whose `line` is a `+` line keeps it. A violation on a context line (inside a diff hunk but not added, e.g. an import left unused by the change) moves to the nearest `+` line **of the same hunk** (smallest distance; on a tie, the earlier line), and its `start_line` is dropped. The claim text keeps the original line number so the comment still says where the problem is. A violation outside every hunk of the file is dropped. Kodus keeps such findings too (its "cites an added line" check only logs); Adus moves them because it posts inline comments only on `+` lines.
3. **Per-Candidate Evidence Collector & Judge (`rules_evidence.rs`):**
   - Bounded by `RULE_EVIDENCE_CONCURRENCY = 4` and a per-review deadline (`deadlines.finder` minus a 10s reserve).
   - **Step A — Evidence Collector:** `fast` at `High`, `RULE_EVIDENCE_MAX_STEPS = 6`, `no_tools_on_last_step = true`. Uses `read_file`/`grep`/`list_dir` and records every successful numbered `read_file` excerpt (`ReadExcerpt { path, content }`). If the collector emits `{"keep": false, "confidence": "high", "rationale": "..."}` citing a `path:line` present in its collected excerpts (`has_source_refutation`), drop immediately.
   - **Step B — Evidence Judge:** Single-turn `fast` at `High` (`RULE_JUDGE_MAX_TOKENS = 1_024`, plus the `High` reasoning reserve) receiving the rule, candidate claim, collected excerpts (deduped, capped at `MAX_RULE_EVIDENCE_CHARS = 24_000`), and related diff patches (capped at `8_000` chars). Outputs `{"verdict": "violated"|"satisfied"|"not_applicable"|"insufficient_evidence", "path": "...", "line": <u32>, "rationale": "..."}`.
   - **Deterministic Citation Gate (`read_contains_line`):** Keep the violation **only** if `verdict == "violated"` AND `line` of `path` literally appears in `excerpts` (prefix `{line}: ` with non-empty text).
4. **Downstream Merge:** Verified rule violations enter the post-verify pipeline as `CandidateFinding` with `severity: Some(rule.severity)`, `needs_verify: false`, `rule_id: Some(rule.id)`, and rule citation in `body`, skipping the HV2 Verifier and proceeding directly to Semantic Dedup → Enrichment → `check_fix`.

---

## 8. Observability

- `AgentOutcome` tracks `gate_refused: bool` and `read_files: HashSet<String>`.
- `review stages finished` info log records `finder_steps`, `gate_refusals`, `synthesis_candidates`, `synthesis_skipped`, `kept_by_origin`, `evidence_gate_runs`, `verify_candidates` (queued), `verifier_kept_unreadable`, `dropped_unverified`, `rule_shards`, `rule_candidates`, `rule_verified`, `budget_used` (weighted), `input_tokens` (raw), `cache_read_tokens`, `cache_write_tokens`, `token_budget_reached`.

---

## 9. Testing & Acceptance

### 9.1 Unit and integration tests (no live LLM)

Extend existing tests and fakes (`crates/review/tests/common/mod.rs`, the scripted `ChatService` fakes in `agent.rs` / `finder.rs` / `verifier.rs`) rather than adding parallel ones. Each test covers a distinct behaviour:

- **Coverage ledger:** range union across several reads; a narrow read leaves the other hunks of the same file pending; a capped read records the shown range from the `[path: lines X-Y of Z]` header; failed reads and same-turn reads do not count; docs and tests never carry debt.
- **Ledger scope:** Generalist and Bug Finder runs with repository tools get a coverage ledger (debt notes and gate); Security, Performance, Pass 2, Verifier and rule-evidence runs get none and receive no debt note.
- **Debt note:** sent only when the pending set changes, kept in the conversation (the next request's messages start with the previous request's messages), never sent during wrap-up.
- **Completion gate:** refuses once and only when all four conditions hold; the second submit is accepted; the held submission is returned when the run later ends unparseable; prose submissions are gated the same way; an accepted empty submission falls back to the latest earlier non-empty findings.
- **Budget bands:** each note is sent once, at the computed step; no note for runs under 6 steps, without repository tools, or with `budget_bands == false` (Verifier and rule-evidence runs).
- **Finder parser:** a `findings` / `dismissed` value sent as a JSON-encoded array string is parsed; any other string is rejected.
- **`<changed_functions>`:** extracted from hunk-header context, trimmed, deduplicated, capped; section omitted when no header has context.
- **Synthesis Rescue:** skipped with the right `synthesis_skipped` reason when budget or time is short; its prompt carries the three context blocks; merge drops exact repeats and keeps new findings; `origin` is set per pass.
- **Verifier:** runs at `High`; the verdict-due note arrives two steps before the end; the final step offers no tools and a prose JSON verdict is parsed; an unreadable verdict after a completed run is recovered or kept, while provider errors and deadline cuts drop the finding; short reads are widened and still capped; the Evidence Gate re-verifies exactly the findings on files the Finder never read, and its verdict wins; more than 25 candidates are all verified (no count cap); `verify_order` puts security before bug before performance before repo_instruction, infers a missing category from the `SpecialistKind`, puts `Pass1` before `ProseRecovery` before `SynthesisRescue` within a category, and keeps the original order for ties.
- **Funnel:** severity fallback to `Warning`; enrichment never changes `body`; `check_fix` strips (never drops) each invalid suggestion shape.
- **Deadlines:** the new fractions, and the compile-time timeout hierarchy still holds.
- **OpenRouter:** host detection; `reasoning` + `require_parameters` body; the "no endpoints found" retry without `require_parameters`, then the effort fallback.
- **Sharded rules** (plan 2): shard building per matching file, lenient parser, context-line violations moved to the nearest `+` line of the same hunk and out-of-hunk ones dropped, citation gate keeps only violations whose `path:line` is in the collected excerpts, and in `Specialists` mode the `Bug` specialist carries `<repository_instructions>` and the `repo_instruction` category.

### 9.2 Live acceptance run (manual, before plan 2 starts)

Re-run `manot40/better-booru` PR #13 with `adus rereview` and `RUST_LOG=info,adus_review=debug,adus_review::trace=trace` three times: in `Generalist` mode with the DeepSeek model through OpenRouter as `primary`, in `Generalist` mode with Claude Sonnet 5.5 as `primary`, and in `Specialists` mode with DeepSeek (the repository's review mode switched in its settings). The four reference defects are: `internal/api/posts.go:165` (orphaned rows reach `PreviewDimExpr`), `internal/image/cache.go` swallowed `_, _ =` update errors, the `s3Storage.Exists` error treated as a miss, and the CDN branch falling through to the hard-coded `"webp"` fallback.

Run it on a pull request with no earlier Adus comments: Dedup drops findings an existing Adus thread already raised (§6 item 1), and PR #13 carries comments from earlier Adus runs. Either open a fresh PR from the same branch, or count each finding the debug log reports as `finding already raised in an existing thread` as a detection.

Pass criteria:

1. **DeepSeek, Generalist:** at least 3 of the 4 reference defects are posted, and the url_builder and migration findings of the earlier runs are still raised as candidates.
2. **Sonnet 5.5, Generalist:** Pass 1 opens every qualifying changed file with `read_file` before its accepted submission, and at least 3 of the 4 reference defects are posted.
3. **DeepSeek, Specialists:** at least 3 of the 4 reference defects are posted; the `Bug` specialist's Pass 1 opens every qualifying changed file before its accepted submission; `Security` and `Performance` receive no coverage debt note and no gate refusal; the run finishes inside `SPECIALISTS_REVIEW_TIMEOUT`.
4. **All three:** no `token_budget_reached`, no `synthesis_skipped` (for `Bug` in Specialists mode), no Finder run ending at its step cap after a gate refusal, and `budget_used`, the raw input total, wall time and `kept_by_origin` recorded in the PR or plan notes to check the §4.3 estimates. If the Specialists run hits the token budget, record which stage was cut (Pass 2 skipped, Verifier candidates dropped); that is the evidence for the follow-up that would raise the default (§4.3).

If a criterion fails, the trace decides which part (coverage, prompt, rescue, Verifier) to tune before plan 2.

---

## 10. Kodus Community Reference (`kodustech/kodus-ai`, behaviour only)

Read at `main`; nothing under `libs/ee/**` or `*.ee.*` was opened:

- `libs/code-review/infrastructure/agents/engine/coverage-ledger.ts` & `libs/agent-harness/infrastructure/policies/completion-gate.policy.ts` — per-hunk range union coverage tracking and per-step trailing coverage debt injection (`readFile` counts; `grep`/`listDir` do not).
- `libs/agent-harness/infrastructure/policies/budget.policy.ts` & `force-finalize.policy.ts` — transition-only `encourage`/`urgent` budget nudges and submit-only final 2 steps.
- `libs/code-review/infrastructure/agents/core/finder.agent.ts` & `finder-output-schema.ts` — lean 5-field Finder schema, prose recovery (`extractFindingsWithRecovery`), Pass 2 `synthesis-rescue` (`<AlreadyInspectedFiles>`, `<RecentInvestigation>`, `<CurrentFindings>`), and Evidence Gate full re-verify for un-opened files.
- `libs/code-review/infrastructure/agents/core/verifier.agent.ts` — 7-step light / 10-step full verify, auto-widening `<20`-line `readFile` calls by `-20/+10`, and `ForceTextFinalizePolicy` (`noToolsOnLastStep: true`).
- `libs/code-review/infrastructure/agents/prompts/prompt-builder.ts` — raw hunks (no expanded context), 500-char `<PRContext>`, 3-lens generalist prompt, adversarial stance.
- `libs/code-review/infrastructure/agents/engine/classify-severity.ts` & `libs/code-review/pipeline/stages/agent-review.stage.ts` — post-verify severity classification, enrichment, and broken-fix downgrade (`checkFix`).
- `libs/code-review/infrastructure/agents/providers/kody-rules-agent.provider.ts`, `kody-rules-sharded.judge.ts`, & `kody-rule-evidence.ts` — per-file sharded rule sweep, 6-step evidence collector with `hasSourceRefutation`, and separate judge with `hasRuleEvidence` (`readContainsLine`).
- `libs/llm/reasoning-options.ts` & `libs/llm/providers/openrouter/index.ts` — OpenRouter `reasoning: { effort }`, and `provider.require_parameters: true` only for models known to reason by default (no retry; the Adus retry in §3.4 is Adus's own).
- Re-checked against `main` at 2026-11-26 (commit `59dedb8`), adding: `libs/agent-harness/infrastructure/policies/force-text-finalize.policy.ts` (no tools on the last step plus a verdict-due note), `core/findings-schema.ts` (stringified `suggestions` arrays from Claude), `providers/base-code-review-agent.provider.ts` (gate and tiers only for PRs over the prompt budget), the verifier's keep-on-unreadable-verdict path in `core/verifier.agent.ts`, and the finder's empty-resubmit fallback in `core/finder.agent.ts`. Also `libs/agent-harness/infrastructure/orchestration/verification-pass.ts` (every candidate verified, four at a time, no count cap), the call in `core/finder.agent.ts` that passes the whole candidate list to it, and `libs/llm/agent-run-context.ts` (the 30-minute per-agent timeout that is Kodus's only bound on verification).

- Compared again against `main` at 2026-11-27 (commit `0cc826f`; review and harness code unchanged since `59dedb8`), behaviour only, adding: `agents/prompts/verifier-prompt.ts` (what is and is not a refutation: cross-file root causes, races and recalled library behaviour are not refutations), `agents/prompts/prompt-builder.ts` and `agents/prompts/review-prompt-blocks.ts` (one file can hold several defects; new branches must still reach their fallback), `agents/engine/dedup-prompt.ts` and `agents/engine/finding-dedup.ts` (conservative grouping, keep both when unsure, per-finding removal logs), `agents/engine/severity-prompt.ts` (fix size does not lower severity) and `agents/engine/enrich-content.ts` (a suggestion replaces exactly the cited lines, as plain code) — the two files `severity.rs` and `enrich.rs` follow, previously uncited; `agents/engine/is-usable-fix.ts` (fix gate; Adus's `check_fix` rules are its own, §6 item 4). The wording Adus took in these areas was rewritten fresh on 2026-11-27 where the comparison found it too close.
---

## 11. Assumptions

1. Synthesis Rescue (Pass 2) runs for all models (gated on remaining Finder deadline & token budget) instead of a hardcoded model-family allowlist.
2. Core Finder uses both per-step coverage debt injection AND a one-shot hard refusal for unread non-doc/non-test code files (`GATE_MIN_STEPS_LEFT = 2`).
3. Existing Adus DB schema (`rules`, `reviews`, `Severity`) stays unchanged with zero migrations; every enabled rule matching `path_glob` runs in per-file shards, and all verified findings are posted.
4. OpenRouter is detected via `ProviderKind::OpenaiCompatible` + `base_url` host `openrouter.ai` (or `*.openrouter.ai`), retrying once without `require_parameters` on `"No endpoints found"`.
5. In `Specialists` mode, Synthesis Rescue runs on `Bug` only; `Security` and `Performance` run Pass 1 only, and Sharded Rules replaces `SpecialistKind::Rules`.
6. `REVIEW_TIMEOUT = 20 min`; `SPECIALISTS_REVIEW_TIMEOUT = 30 min`; `JOB_TIMEOUT = 31 min`; `cache_read_tokens` weighted at `/ 4`; `small_pr` fast path stays unchanged.
7. Uncommitted live tracing (`ChatResponse::reasoning()` and `adus_review::trace` spans) is kept and cleaned of `TEMP(experiment)` hacks; Claude `display: "summarized"` and the hidden `rereview` command stay as permanent, documented behaviour (§3.1).
8. Split into two sequential implementation plans: Core Loop (`2026-11-25-review-engine-core-loop.md`) first, then Sharded Rules (`2026-11-25-review-engine-sharded-rules.md`). Plan 2 starts only after the §9.2 acceptance run passes (or its failures are triaged).
9. The coverage debt note is append-only (kept in the conversation, sent only when the pending set changes), because a request-only message would edit the history that Claude models with preserved thinking check.
10. Stage deadline fractions move to Finder 55 % / Verifier 80 % / Post-verify 93 % for both review timeouts; the 2 M default token budget is unchanged and is re-checked against the §9.2 run's recorded usage.
11. Enrichment never rewrites the verified Finder `body`; it only adds a title and an optional suggestion.
12. `<changed_functions>` comes from git hunk-header context only (no AST); it may be empty for some languages or providers, and the prompt treats it as a hint.
13. `review_log` (formerly `reasoning`) is required in the `submit_findings` schema but its absence is logged, never refused.
14. (User decisions 2026-11-26.) The Verifier runs on `primary` at `High`; Sharded Rule Judge, Evidence Collector and Evidence Judge run on `fast` at `High`; rule violations on context lines move to the nearest `+` line of the same hunk.
15. In `Specialists` mode the `Bug` specialist takes over repository-instruction checks from the removed `Rules` specialist.
16. A completed Verifier run with an unreadable verdict keeps the finding; only provider errors, deadline cuts and runs with no final reply drop it.
17. Only `Generalist` and `Bug` Pass 1 runs with repository tools get a coverage ledger (debt notes and gate); `Security` and `Performance` get neither. The 2 M token default is kept for `Specialists` mode too, though its estimate comes close; the §9.2 Specialists run measures it.
18. (User decision 2026-11-26.) The Verifier has no candidate count cap; the Verifier deadline and the token budget bound it, and `verify_order` (category, then origin, then original position) decides which candidates they cut. The category ranking is Adus's own; Kodus verifies in Finder order.
19. (2026-11-27, after the Kodus comparison.) The Verifier no longer judges suggestions (`drop_suggestion` removed); its refutation rules gain the cross-file, concurrency and recalled-library clauses and a narrowed hedge rule; Dedup keeps both findings when unsure; the Finder prompt adds the several-defects-per-file rule and the per-branch fallback check. The §9.2 run uses a PR without earlier Adus comments, or counts logged `already_raised` findings as detections.

---

## 12. SPEC.md Sync

Update `docs/SPEC.md` §3.1 (hidden debug-only `rereview` command), §6.1 (OpenRouter reasoning & `require_parameters`, Claude `display: "summarized"`), §7 (`REVIEW_TIMEOUT` 20 min, stage deadline fractions 55 / 80 / 93 %), §8.3 (removal of `expand_diff_block`, Verifier auto-widen), §8.4 (raw diffs, 500-char PR description, `<changed_functions>`, per-hunk Coverage Ledger with append-only debt note & one-shot gate (Generalist and Bug only), escalating budget bands and their texts, required `reasoning`, Synthesis Rescue with its budget/time gate and `origin`, Prose Recovery, `primary`/`High` Verifier with the verdict-due note, no `MAX_VERIFY_CANDIDATES` cap and the `verify_order` queue (replacing the §7 "Up to `MAX_VERIFY_CANDIDATES = 25` sorted `Critical > Warning > Info`" text), keep-on-unreadable-verdict semantics and the Evidence Gate, stringified-array parsing and the empty-resubmit fallback, `Bug` specialist carrying repository instructions in `Specialists` mode, post-verify Severity / title-and-suggestion Enrichment / `check_fix`, parallel Sharded Rules + Evidence Collector/Judge on `fast`/`High` with nearest-`+`-line anchoring, cache-weighted `TokenBudget`), and §10 (archive entry), plus `crates/adus/src/worker.rs` timeout comment.
