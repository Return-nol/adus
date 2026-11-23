# PR Discussion Awareness & Learning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reviews skip findings already raised in the PR's discussion, and closed PRs teach Adus "by design" exceptions as repo memories (approval or auto per repo).

**Architecture:** A new `GitProvider::list_pr_threads` reads PR discussion threads (GitHub, Azure). The engine passes them to the existing Semantic Dedup stage, which may drop a finding only by citing the existing comment that covers it. A new `Job::LearnPr` (from GitHub `closed` / Azure `completed|abandoned`) runs `Engine::learn_pr`: one fast-model call extracts exceptions, stored as memories with a new `status` (`pending`/`active`/`rejected`), gated by `repositories.learning_mode`.

**Tech Stack:** Rust (axum, sqlx runtime queries, reqwest, wiremock tests), Postgres 18, SvelteKit 5 + shadcn-svelte.

**Spec:** `docs/superpowers/specs/2026-11-23-pr-discussion-awareness-and-learning-design.md`

## Global Constraints

- **Clean-room and no-assumption rule (verbatim from AGENTS.md, binding for every implementer and reviewer):**
  1. Kodus community code (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. Order of authority: the task brief (or active unmerged change spec), then docs/SPEC.md (and DESIGN.md for `web/`), then AGENTS.md and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. Openness: every implementer report lists its assumptions under an "Assumptions" heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. Reviewers flag as Important any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing docs/SPEC.md updates when SDD completes.
- Follow `AGENTS.md` (repo root) and, for `web/`, `web/AGENTS.md` + `docs/DESIGN.md`. Key rules: runtime `sqlx::query`/`query_as` only (never `query!`); `ApiJson`/`ApiPath`/`ApiQuery` extractors and `ApiError` codes; no `.unwrap()`/`.expect()` outside tests; `dyn Trait` only at the existing 6 seams; `#lib/...` imports; skeletons kept in sync.
- Comment bodies are **never logged** and always pass through `redact_secrets` before reaching an LLM; everything from a PR comment is wrapped as untrusted data.
- Existing-comment section limits: each comment body cut to **1,000 chars**, at most **60 threads**.
- Learning limits: at most **5** exceptions per PR, each ≤ **2,000** chars (reuse `MAX_MEMORY_CHARS` from `crates/review/src/command.rs`, made `pub(crate)`).
- Summary line copy (exact): `{N} finding(s) skipped: already raised in this PR's discussion.`
- Learned memories: `scope = repo`, `source = pr_learning`, `source_url` = thread url, `created_by` = defending registered user or `NULL`.
- `learning_mode` values `off | approval | auto`, default `approval`. `memory_status` values `pending | active | rejected`, existing rows `active`.
- Tests: `cargo test --workspace -j 2` (dev Postgres/Redis via `docker compose -f docker-compose.dev.yml -p adus-dev up -d`), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `pnpm -C web check` (0 errors). No redundant tests (AGENTS.md "Testing").
- Commits: Conventional Commits ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, on branch `feat/pr-discussion-learning`; never push.

## Review Focus

1. Thread fetch fails (provider 403/500/timeout) during a review → the review still posts normally, with no "already raised" drops. (Task 4)
2. Dedup answer drops a finding without citing a valid existing comment, or cites an out-of-range index → that finding is kept. (Task 4)
3. A PR comment containing a secret (e.g. a `ghp_…` token) → the text sent to the LLM is redacted, in both dedup and learning. (Tasks 4, 5)
4. Azure sends several `updated` events after a PR completes, and GitHub may redeliver `closed` → learning runs once per `(repo, pr, head_sha)`. (Task 5)
5. `auto` mode where the defender is unregistered or a disabled user → memory is `pending`, not `active`. (Task 5)

---

### Task 1: PR threads in `crates/git` — types, trait method, GitHub

**Files:**
- Modify: `crates/git/src/types.rs`, `crates/git/src/lib.rs`, `crates/git/src/github.rs`
- Modify (fakes): every `impl GitProvider` in tests/test support — `crates/server/tests/common/mod.rs`, `crates/review/tests/common/mod.rs`, `crates/review/src/test_support.rs` (if it implements `GitProvider`); grep `impl GitProvider for` to find all
- Test: `crates/git/tests/github.rs`, fixtures under `tests/fixtures/github/` (follow the existing fixture layout)

**Interfaces:**
- Produces (exported from `adus_git`):
  ```rust
  pub struct PrThread { pub path: Option<String>, pub line: Option<u32>, pub status: Option<String>, pub url: Option<String>, pub comments: Vec<PrThreadComment> }
  pub struct PrThreadComment { pub author: String, pub by_adus: bool, pub body: String }
  // GitProvider (required method, no default):
  async fn list_pr_threads(&self, repo: &RepoRef, pr_number: i64) -> Result<Vec<PrThread>, GitError>;
  ```
  Derive `Debug, Clone, PartialEq`. `path` is `normalize_path`-ed; `url` only http(s); comments oldest first. `by_adus` = body contains `<!-- adudu -->` (use the existing marker constant if one is reachable from `crates/git`; otherwise the literal).
- Fakes: add a `threads: Vec<PrThread>` and `threads_error: Option<GitError>` to each fake's state, returned by `list_pr_threads`.

- [ ] **Step 1: Write failing wiremock tests** in `crates/git/tests/github.rs`:
  - `list_pr_threads_groups_review_comments_by_reply`: mocks `GET /repos/o/r/pulls/7/comments` (two root comments, one reply with `in_reply_to_id` of the first), `GET /repos/o/r/issues/7/comments` (one comment), `GET /repos/o/r/pulls/7/reviews` (one review with non-empty body, one with empty body). Assert: 4 threads; the review-comment thread has `path == Some("src/a.rs")`, `line == Some(12)`, 2 comments oldest first with the right authors; issue comment and review body threads have `path == None`; the empty review body yields no thread.
  - `list_pr_threads_skips_bot_users_and_flags_adus`: a comment from `user.type == "Bot"` is absent; a comment whose body contains `<!-- adudu -->` has `by_adus == true`.
  - `list_pr_threads_401_is_unauthorized`.
- [ ] **Step 2:** Run `cargo test -p adus-git --test github list_pr_threads -j 2` → FAIL (method missing).
- [ ] **Step 3:** Add the types and trait method; implement for `GithubProvider` with the existing `get_pages` (pagination cap, host check) and `send` error mapping. GitHub review comments: `line` (new side; `None` when null/outdated), `html_url` as `url`. Update all fakes.
- [ ] **Step 4:** Run `cargo test -p adus-git -j 2` and `cargo build --workspace --tests -j 2` → PASS / builds.
- [ ] **Step 5: Commit** `feat(git): list PR discussion threads (GitHub)`.

### Task 2: PR threads — Azure

**Files:**
- Modify: `crates/git/src/azure.rs`
- Test: `crates/git/tests/azure.rs`, fixture `tests/fixtures/azure/threads.json`

**Interfaces:**
- Consumes: `PrThread`, `PrThreadComment`, `GitProvider::list_pr_threads` (Task 1).

- [ ] **Step 1: Write failing tests:**
  - `list_pr_threads_maps_context_status_and_comments`: `GET {org}/{project-guid}/_apis/git/repositories/{repo-guid}/pullRequests/7/threads` returns: a file thread (`threadContext.filePath: "/src/a.rs"`, `rightFileStart.line: 5`, `status: "byDesign"`, two text comments), a general thread (no `threadContext`), a thread with only a `commentType: "system"` comment, and a thread whose only comment `isDeleted: true`. Assert: 2 threads; first has `path == Some("src/a.rs")`, `line == Some(5)`, `status == Some("byDesign")`, authors from `author.uniqueName`; general thread `path == None`.
  - `list_pr_threads_203_is_unauthorized`.
- [ ] **Step 2:** Run `cargo test -p adus-git --test azure list_pr_threads -j 2` → FAIL.
- [ ] **Step 3:** Implement with the existing `get_json`/`send`/`repo_url` helpers (GUIDs in the URL, same `api-version` as other PR calls). Thread `url`: `{repository.webUrl}/pullrequest/{n}?discussionId={thread id}` when a web URL is known (the PR response or repo data you already fetch), else `None` — do not add an extra request just for the url.
- [ ] **Step 4:** Run `cargo test -p adus-git -j 2` → PASS.
- [ ] **Step 5: Commit** `feat(git): list PR discussion threads (Azure)`.

### Task 3: Migration 0009 and DB layer

**Files:**
- Create: `migrations/0009_pr_learning.sql`, `crates/db/src/learning_runs.rs`
- Modify: `crates/core/src/types.rs` (add `LearningMode`), `crates/db/src/memories.rs`, `crates/db/src/repositories.rs`, `crates/db/src/lib.rs`
- Test: `crates/db/tests/memories.rs` (create if absent), `crates/db/tests/repositories.rs`, `crates/db/tests/learning_runs.rs`

**Interfaces:**
- Produces:
  - `adus_core::types::LearningMode { Off, Approval, Auto }` — `#[default] Approval`, serde/sqlx `snake_case`, sqlx type `learning_mode` (mirror `ReviewMode`).
  - `adus_db::memories::MemorySource::PrLearning`; `MemoryStatus { Pending, Active, Rejected }` (sqlx type `memory_status`); `Memory.status: MemoryStatus`; `NewMemory.status: MemoryStatus`.
  - `memories::get(pool, id: Uuid) -> Result<Option<Memory>>`
  - `memories::list(pool, repository_id: Option<Uuid>, status: Option<MemoryStatus>) -> Result<Vec<Memory>>` and `memories::list_all(pool, status: Option<MemoryStatus>)` — `None` = every status except `rejected`.
  - `memories::approve(pool, id, content: Option<&str>) -> Result<Option<Memory>>` (only `pending` → `active`; `None` if missing or not pending)
  - `memories::reject(pool, id) -> Result<Option<Memory>>` (only `pending` → `rejected`)
  - `memories::applicable` adds `AND status = 'active'`.
  - `memories::for_learning(pool, repository_id: Uuid) -> Result<Vec<Memory>>`: global + that repo's memories, every status, enabled or not.
  - `Repository.learning_mode: LearningMode`; `RepoPatch.learning_mode: Option<LearningMode>`.
  - `adus_db::learning_runs::{LearningOutcome, NewLearningRun, exists, record}`:
    `LearningOutcome { Learned, NothingToLearn, Skipped, Failed }` stored as text `learned | nothing_to_learn | skipped | failed`;
    `exists(pool, repository_id, pr_number: i64, head_sha: &str) -> Result<bool>`;
    `record(pool, NewLearningRun { repository_id, pr_number, head_sha: &str, outcome, learned_count: i32 }) -> Result<()>` with `ON CONFLICT (repository_id, pr_number, head_sha) DO NOTHING`.

- [ ] **Step 1: Migration** `0009_pr_learning.sql` exactly as spec §6 (sqlx runs it in one transaction; `ADD VALUE` is fine there because nothing in this migration uses `'pr_learning'`).
- [ ] **Step 2: Write failing tests:**
  - `applicable_excludes_pending_and_rejected`: insert active, pending, rejected repo memories → `applicable` returns only the active one.
  - `approve_and_reject_only_from_pending`: approve pending with new content → status active + content changed; approving again → `None`; reject an active one → `None`; reject pending → `rejected`.
  - `list_hides_rejected_unless_asked`: `list(.., None)` omits rejected; `list(.., Some(Rejected))` returns only it.
  - `repo_learning_mode_defaults_to_approval_and_patches` (in repositories tests, extend the existing patch test if one covers `review_mode`).
  - `learning_run_record_is_idempotent`: `record` twice for the same key → one row, `exists` true; different `head_sha` → `exists` false.
- [ ] **Step 3:** Run `cargo test -p adus-db -j 2` → FAIL.
- [ ] **Step 4:** Implement; update every `NewMemory` construction site (`command.rs`, server API) to pass `status: MemoryStatus::Active`.
- [ ] **Step 5:** Run `cargo test --workspace -j 2` → PASS.
- [ ] **Step 6: Commit** `feat(db): memory status, learning mode and learning runs (migration 0009)`.

### Task 4: Discussion-aware Semantic Dedup (Part 1)

**Files:**
- Create: `crates/review/src/discussion.rs` (thread selection + formatting shared by dedup and learning)
- Modify: `crates/review/src/dedup.rs`, `crates/review/src/engine.rs`, `crates/review/src/lib.rs`
- Test: unit tests in `dedup.rs` / `discussion.rs`; `crates/review/tests/engine.rs`

**Interfaces:**
- Consumes: `GitProvider::list_pr_threads`, fakes' `threads` / `threads_error` (Task 1).
- Produces:
  - `discussion::threads_for_dedup<'a>(threads: &'a [PrThread], paths: &BTreeSet<String>) -> Vec<&'a PrThread>`: threads whose `path` is in `paths` plus general threads; same-file first, then newest first; at most 60.
  - `discussion::format_threads(threads: &[&PrThread]) -> String`: JSON-ish indexed list (`index`, `path`, `line`, `status`, comments with `index`, `author` — `"adus"` when `by_adus` — and body), each body `redact_secrets`-ed and cut to 1,000 chars. Callers wrap it with `prompt::wrap`.
  - `dedup::deduplicate_findings(chat, provider_id, model, usage_ctx, token_budget, findings: Vec<CandidateFinding>, existing: &[&PrThread]) -> DedupOutcome` where `pub(crate) struct DedupOutcome { pub kept: Vec<Suggestion>, pub already_raised: usize }`.
  - Model answer: `{"keep_indices": [...], "already_raised": [{"finding": i, "comment": j}]}` — `comment` is the thread index in `<existing_comments>`. A finding is dropped as already raised only when listed with a valid `j`; listed with an invalid `j` → kept. `keep_indices` keeps today's semantics for duplicates among findings. Invalid/unparseable answer or error → keep all, `already_raised = 0`.

- [ ] **Step 1: Write failing unit tests** (use `ScriptedChat`):
  - `dedup_runs_for_one_finding_when_existing_comments`: 1 finding + 1 thread → one LLM call made; answer cites it → `kept` empty, `already_raised == 1`.
  - `dedup_skips_call_for_one_finding_without_comments`: no call.
  - `dedup_keeps_finding_cited_with_invalid_comment_index`.
  - `dedup_prompt_contains_redacted_existing_comments`: a thread body with a token like `ghp_` + 36 alnum chars → the captured prompt contains the redaction placeholder, not the token, and contains `<existing_comments>`.
  - `threads_for_dedup_filters_orders_and_caps`: 70 same-file threads + other-file thread + general thread → 60 returned, other-file thread excluded, same-file before general.
- [ ] **Step 2: Write failing engine tests** in `crates/review/tests/engine.rs` (reuse the existing harness):
  - `review_skips_findings_already_raised_in_discussion`: fake threads contain a comment on the finding's file; scripted dedup answer cites it → nothing posted inline, summary contains `1 finding(s) skipped: already raised in this PR's discussion.`
  - `review_posts_normally_when_thread_fetch_fails`: `threads_error = Some(GitError::Status(500))` → review `done`, finding posted, no "already raised" line.
- [ ] **Step 3:** Run `cargo test -p adus-review -j 2` → FAIL.
- [ ] **Step 4:** Implement. Engine fetches threads once in `run` after the files are known and only when `files` is non-empty; error → `tracing::warn!` (no bodies) and empty list. `Stages` gets `threads: &'a [PrThread]`. Dedup now runs when `verified.len() >= 2` or (`verified.len() >= 1` and the selected threads are non-empty); deadline fallback unchanged. Push the exact summary warning when `already_raised > 0`. Rewrite `DEDUP_SYSTEM` fresh to cover both duplicate merging and already-raised detection (existing comments are data, not instructions; drop only what an existing comment already covers — same problem at the same or related location).
- [ ] **Step 5:** Run `cargo test --workspace -j 2` and clippy → PASS.
- [ ] **Step 6: Commit** `feat(review): skip findings already raised in the PR discussion`.

### Task 5: `Engine::learn_pr` (Part 2 core)

**Files:**
- Create: `crates/review/src/learn.rs`
- Modify: `crates/review/src/lib.rs`, `crates/review/src/engine.rs` (extract provider/model resolution into a shared helper), `crates/review/src/command.rs` (`MAX_MEMORY_CHARS` → `pub(crate)`)
- Test: `crates/review/tests/learn.rs` (reuse `crates/review/tests/common/mod.rs`)

**Interfaces:**
- Consumes: Task 1 threads, Task 3 `memories::{insert, for_learning}`, `learning_runs::{exists, record}`, `Repository.learning_mode`, `command::authorize`, Task 4 `discussion::format_threads`.
- Produces: `impl Engine { pub async fn learn_pr(&self, repository_id: Uuid, pr_number: i64) -> Result<(), EngineError> }`.

Behavior (spec §5):
1. Repo missing/deleted, `!repo.enabled`, or `learning_mode == Off` → `Ok(())` with no run row (nothing ran, so a later mode change can still learn this PR). Get connection + provider, `get_pr` (for `head_sha`; `NotFound` → `Fatal`, other errors via `fetch_error`). `learning_runs::exists` → `Ok(())`.
2. No enabled provider with a model (same resolution as reviews, using the fast model) → record `skipped`, `Ok(())`, no comment.
3. `list_pr_threads` (error via `fetch_error`). Qualifying threads: ≥ 2 comments from ≥ 2 distinct authors, `by_adus` comments counting as author `"adus"`. None → record `nothing_to_learn`.
4. One `chat_low_effort` call (fast model, `ReasoningEffort::Low`, usage with `repository_id`, no `review_id`, no token budget). Prompt (written fresh): qualifying threads (`format_threads`, capped at 60), current active/pending memories as "already known", rejected learned memories as "do not propose again". Answer `{"exceptions": [{"content", "thread", "defended_by": [...]}]}`.
5. Discard exceptions with invalid `thread`, no valid `defended_by`, or empty content; truncate list to 5; content longer than 2,000 chars → discarded.
6. Status: `approval` → `Pending`; `auto` → `Active` only if any `defended_by` comment author resolves through `command::authorize` (GitHub login / Azure email; disabled users rejected) to a user — else `Pending`. `created_by` = that user's id or `None`. Insert with `source: PrLearning`, `source_url` = thread url.
7. Record `learned` with `learned_count` (0 valid exceptions → `nothing_to_learn`). Unparseable answer / `ChatError::Invalid` → record `failed`, `Ok(())`. `AllKeysFailed`/`NoKeys`/`Internal` → `EngineError::Retryable` without a row.

- [ ] **Step 1: Write failing tests** in `crates/review/tests/learn.rs`:
  - `learn_pr_approval_mode_creates_pending_memory`: one qualifying thread; scripted answer with one exception → one memory `Pending`, `PrLearning`, `source_url` = thread url; run row `learned`, count 1.
  - `learn_pr_auto_mode_activates_only_for_registered_defender`: two exceptions, one defended by a registered user's GitHub login (→ `Active`, `created_by` set), one by an unknown login (→ `Pending`).
  - `learn_pr_auto_mode_disabled_user_stays_pending`.
  - `learn_pr_runs_once_per_head`: call twice → one LLM call, one row, memories not duplicated.
  - `learn_pr_without_qualifying_threads_makes_no_llm_call`: single-author thread only → no call, row `nothing_to_learn`.
  - `learn_pr_off_mode_does_nothing`: no call, no row.
  - `learn_pr_prompt_lists_rejected_and_redacts_secrets`: a rejected learned memory appears in the prompt under the do-not-propose section; a token in a comment is redacted.
  - `learn_pr_invalid_answer_records_failed`.
- [ ] **Step 2:** Run `cargo test -p adus-review --test learn -j 2` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace -j 2`, clippy → PASS.
- [ ] **Step 5: Commit** `feat(review): learn by-design exceptions from closed PRs`.

### Task 6: `Job::LearnPr`, webhooks and worker wiring

**Files:**
- Modify: `crates/core/src/job.rs`, `crates/server/src/webhooks.rs`, `crates/adus/src/worker.rs`
- Test: unit tests in `job.rs` and `webhooks.rs`; `crates/adus/tests/worker.rs` only if an existing dispatch test can be extended

**Interfaces:**
- Consumes: `Engine::learn_pr` (Task 5).
- Produces: `Job::LearnPr { repository_id: Uuid, pr_number: i64 }` (serde tag `learn_pr`).

- [ ] **Step 1: Write failing tests:**
  - In `job.rs`: extend `job_json_roundtrip` with a `LearnPr` value (`"type": "learn_pr"`).
  - In `webhooks.rs`: change the existing case table so `("closed", None)` becomes `("closed", Some(Job::LearnPr { .. }))` for GitHub `pull_request`; extend the Azure `completed`/`abandoned` test so both yield `LearnPr` for `git.pullrequest.updated`.
- [ ] **Step 2:** Run `cargo test -p adus-core -p adus-server -j 2` → FAIL.
- [ ] **Step 3:** Implement. Worker: `run` dispatches `LearnPr` to `learn_pr` under the same per-PR lock key as reviews; the log label is `learn_pr`; `gave_up` does nothing for it.
- [ ] **Step 4:** Run `cargo test --workspace -j 2`, clippy → PASS.
- [ ] **Step 5: Commit** `feat: queue a learning job when a PR closes`.

### Task 7: API — memory status, approve/reject, learning mode

**Files:**
- Modify: `crates/server/src/api/memories.rs`, `crates/server/src/api/repositories.rs`
- Test: `crates/server/tests/memories.rs` (or the existing memories test file), `crates/server/tests/repositories.rs` (or wherever repository PATCH is tested)

**Interfaces:**
- Consumes: Task 3 DB functions and types.
- Produces:
  - `MemoryJson` gains `status` (`"pending" | "active" | "rejected"`) and `source` (if not already present, incl. `"pr_learning"`).
  - `GET /api/v1/memories?status=pending|active|rejected` (absent = all except rejected; invalid → 422 `validation_error`); the existing scope/repo filters keep working.
  - `POST /api/v1/memories/{id}/approve` body `{"content"?: string}` (`AdminUser`; content trimmed, non-empty, ≤ 2,000 chars else 422) → 200 memory; missing → 404 `not_found`; not pending → 409 `invalid_status`.
  - `POST /api/v1/memories/{id}/reject` (`AdminUser`) → 200 memory; same 404/409.
  - `PATCH` / `DELETE /api/v1/memories/{id}`: if the memory is `pending` or `rejected` and the caller is not admin → 403 `forbidden`; otherwise unchanged.
  - Repositories JSON gains `learning_mode`; repo PATCH accepts `learning_mode` (admin only, as `review_mode` today).

- [ ] **Step 1: Write failing tests:** `approve_requires_admin_and_pending` (user → 403; admin on pending → 200 + `active`; admin again → 409), `reject_marks_rejected_and_hides_from_default_list`, `user_cannot_patch_or_delete_pending_memory` (403 both) while `user_can_still_patch_active_memory`, `list_filters_by_status` (+ invalid status 422), `repository_learning_mode_round_trip` (default `approval`, admin PATCH to `auto`, user PATCH 403).
- [ ] **Step 2:** Run `cargo test -p adus-server -j 2` → FAIL.
- [ ] **Step 3:** Implement with `ApiJson`/`ApiPath`/`ApiQuery` and `ApiError`.
- [ ] **Step 4:** Run `cargo test --workspace -j 2`, clippy → PASS.
- [ ] **Step 5: Commit** `feat(server): memory approval API and repository learning mode`.

### Task 8: Dashboard — pending memories and learning mode

**Files:**
- Modify: `web/src/routes/rules/+page.svelte` (memory section), `web/src/routes/repositories/+page.svelte`, the memory dialog in `web/src/lib/components/` if editing pending content needs it, API types used by these pages

**Interfaces:**
- Consumes: Task 7 endpoints and JSON fields.

- [ ] **Step 1:** Memory section: a "Pending" tab/group with a count badge (all repos, `GET /memories?status=pending` plus the global/repo views as today), each item showing content, repo `display_name` (server-provided), a "View thread" link (`source_url`), and Approve / Edit-then-approve / Reject buttons for admins only (role `user` sees it read-only). Reject uses an alert-dialog. A "Rejected" view (admins) lists rejected learned memories with Delete. Active learned memories show a "Learned from PR" badge. Toasts via sonner.
- [ ] **Step 2:** Repositories page: a "Learning" select per repo (`Off` / `Needs approval` / `Automatic`), admin only, next to the review-mode control; PATCH on change with toast.
- [ ] **Step 3:** Update the affected skeletons in the same change (web/AGENTS.md "Loading States").
- [ ] **Step 4:** Run `pnpm -C web check` → 0 errors; `pnpm -C web build` → success; `pnpm -C web exec prettier --check src` (or the repo's `fmt:check` script) → clean. No browser automation; say so in the report.
- [ ] **Step 5: Commit** `feat(web): approve learned memories and set learning mode per repo`.

### Task 9: Sync `docs/SPEC.md` and mark spec implemented

**Files:**
- Modify: `docs/SPEC.md`, `docs/superpowers/specs/2026-11-23-pr-discussion-awareness-and-learning-design.md`, `AGENTS.md` only if a cross-cutting guardrail changed, `docs/superpowers/backlog.md` for deferred minors

- [ ] **Step 1:** Update `docs/SPEC.md`: schema (§3.1, migrations `0001`–`0009`, `memory_status`, `learning_mode`, `pr_learning_runs`), `GitProvider::list_pr_threads` (§5), webhooks closed/completed → `LearnPr` (§5/§7), worker job kinds (§7), engine Dedup with existing comments + summary line (§8.4), learning job (new §8.x), memories API + approval rules (§4), dashboard (§9), product decision on learned memories (§1).
- [ ] **Step 2:** Set the dated spec's status line to `Status: implemented` and add its entry to `docs/SPEC.md` §10.
- [ ] **Step 3:** Commit `docs: sync SPEC.md for PR discussion awareness and learning`.
