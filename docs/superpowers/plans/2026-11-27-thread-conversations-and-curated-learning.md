# Thread Conversations & Curated Learning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Adus comments are visibly labelled as the bot; Adus converses in its own finding threads (answers replies, validates fixes, sets thread status); closed PRs are curated into rich memories (convention / exception / fix pattern) that shape later reviews.

**Architecture:** Every finding thread Adus posts is recorded in a new `review_threads` table using the provider ids that `post_review` now returns. Three new `GitProvider` methods (`reply_in_thread`, `set_thread_status`, `get_thread`) let a new `Job::ThreadReply` and a push-validation step in `ReviewPr` answer, validate and resolve threads; outcomes are stored immediately. `learn_pr` becomes a curator (review model) reading those outcomes plus human threads, writing memories with `kind` and curated fields; prompts group memories by kind, the Finder actively looks for fix patterns, and code turns a cited `precedent` into an Info downgrade (exception) or a precedent block (fix pattern).

**Tech Stack:** Rust (axum, sqlx runtime queries, reqwest, wiremock), Redis Streams queue, Postgres 18, SvelteKit 5 + shadcn-svelte.

**Spec:** `docs/superpowers/specs/2026-11-27-thread-conversations-and-curated-learning-design.md` (read it whole; section numbers below refer to it).

## Global Constraints

- **Clean-room and no-assumption rule (verbatim from AGENTS.md, binding for every implementer and reviewer):**
  1. Kodus community code (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. Order of authority: the task brief (or active unmerged change spec), then docs/SPEC.md (and DESIGN.md for `web/`), then AGENTS.md and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. Openness: every implementer report lists its assumptions under an "Assumptions" heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. Reviewers flag as Important any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing docs/SPEC.md updates when SDD completes.
- Follow `AGENTS.md` and, for `web/`, `web/AGENTS.md` + `docs/DESIGN.md`: runtime `sqlx::query`/`query_as` only; `ApiJson`/`ApiPath`/`ApiQuery` + `ApiError`; no `.unwrap()`/`.expect()` outside tests; `dyn Trait` only at the existing 6 seams; `#lib/...` imports; skeletons in sync; no browser automation (verify UI with `pnpm -C web check` + `curl`).
- Comment bodies are **never logged**, pass through `redact_secrets` before any LLM call or DB write, and are wrapped as untrusted data (`prompt::wrap`).
- Wire formats (GitHub REST/GraphQL, Azure DevOps 7.1) come from public provider docs; each new endpoint gets a fixture under `tests/fixtures/{github,azure}/` in the real payload shape.
- Exact values from the spec: `max_thread_replies` default **5** (0 = off), `max_validate_threads` default **30** (0 = off), `max_learned_per_pr` default **10** (≥ 1), `curator_effort` default **`medium`**; memory `content` and each of `context`/`reason`/`resolution` ≤ **2,000** chars (`MAX_MEMORY_CHARS`); `evidence` ≤ **10** entries, http(s) only; threads section ≤ **40,000** chars; memory lists ≤ **20,000** chars each.
- Label copy (spec §4.1): inline `🤖 **Adudu** · ⚠️ **Warning:** <title>` (severity label as today); summary first line `🤖 **Adudu** reviewed this PR`; replies `🤖 **Adudu**` on its own line. Marker always first: `MARKER + "\n" + label…`.
- Summary line copy (exact): `{N} earlier finding(s) confirmed fixed.`
- Precedent copy (exact): exception — `A similar case was declared {by design|won't fix} in PR #{N}: {reason}`; fix pattern — `A similar case was fixed in PR #{N}: {resolution}`; each followed by the evidence link when there is one.
- Thread replies and push validation: provider fast model, `ReasoningEffort::Low`. Curator: provider primary (review) model, effort from `settings.curator_effort`.
- Tests: `cargo test --workspace -j 2` (dev DB: `docker compose -f docker-compose.dev.yml -p adus-dev up -d`), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `pnpm -C web check` (0 errors). No redundant tests; reuse `FakeGit`/`FakeGitFactory`, `crates/review/src/test_support.rs`, `#[sqlx::test(migrations = "../../migrations")]`.
- Commits: Conventional Commits ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, on branch `feat/thread-conversations`; never push.

## Review Focus

1. A human reply that quotes an Adus comment (marker mid-body, e.g. `> <!-- adudu -->…`) → treated as human: triggers `ThreadReply`, counted as human in learning. (Task 1)
2. A reply and a push for the same fix land in either order → exactly one Adus "fixed" reply, thread `fixed` once. (Tasks 7, 8)
3. Azure `updated` events with an unchanged head (title edit, vote, Adus's own status change) → no LLM call from validation. (Task 8)
4. Model answers `by_design` / `wont_fix` with an empty reason, or a curator memory of kind `exception` without `reason` → treated as `needs_reason` / dropped. (Tasks 7, 10)
5. Thread reply cap reached, or `max_thread_replies = 0` → no reply, no LLM call, job succeeds. (Task 7)

---

### Task 1: Marker-at-start recognition + bot label

**Files:**
- Modify: `crates/core/src/types.rs`, `crates/git/src/github.rs`, `crates/git/src/azure.rs`, `crates/server/src/webhooks.rs`, `crates/review/src/command.rs`, `crates/review/src/prompt.rs`, `crates/review/src/engine.rs` (no-model notice)
- Test: existing tests in `crates/core/src/types.rs`, `crates/review/src/prompt.rs`, `crates/server/src/webhooks.rs`

**Interfaces:**
- Produces: `adus_core::types::is_adus_comment(body: &str) -> bool` (trimmed-start body starts with `ADUDU_MARKER`); `crate::prompt::adus_body(label: &str, text: &str) -> String` returning `format!("{MARKER}\n{label}\n\n{text}")` when `text` non-empty, else `format!("{MARKER}\n{label}")`; `crate::prompt::REPLY_LABEL: &str = "🤖 **Adudu**"`.

- [ ] **Step 1: Failing tests**
  - `types.rs` `is_adus_comment_requires_marker_first`: `true` for `"<!-- adudu -->\nx"` and `"  \n<!-- adudu -->"`; `false` for `"> <!-- adudu -->\nquoted"` and `"ok <!-- adudu -->"`.
  - `prompt.rs`: extend `format_inline` test — body starts with `"<!-- adudu -->\n🤖 **Adudu** · ⚠️ Warning:"`; extend summary test — second line is `🤖 **Adudu** reviewed this PR`.
  - `webhooks.rs`: a GitHub `issue_comment` whose body is `"> <!-- adudu -->\nthanks @adudu review"` yields `Job::PrComment` (today it is dropped).
- [ ] **Step 2:** `cargo test -p adus-core -p adus-review -p adus-server -j 2` → the new assertions fail.
- [ ] **Step 3:** Add `is_adus_comment`; replace every `contains(ADUDU_MARKER)` / `contains(MARKER)` (github.rs `by_adus`, azure.rs `by_adus`, webhooks.rs ×2, command.rs `parse_command`) with it. Change `format_inline` first line to `🤖 **Adudu** · {severity_label}: {title}` keeping bold as today, `format_summary` heading to `🤖 **Adudu** reviewed this PR`, command replies and the no-model notice to `adus_body(REPLY_LABEL, …)`.
- [ ] **Step 4:** Same command → PASS; full `cargo test --workspace -j 2` green (update existing assertions that pinned the old heading).
- [ ] **Step 5:** Commit `feat: label Adus comments and recognise them by a leading marker`.

### Task 2: Migration 0010 + DB modules

**Files:**
- Create: `migrations/0010_thread_conversations.sql`, `crates/db/src/review_threads.rs`
- Modify: `crates/db/src/lib.rs`, `crates/db/src/memories.rs`, `crates/db/src/settings.rs`
- Test: `crates/db/tests/` (follow the existing per-module test files; grep `sqlx::test` in `crates/db`)

**Interfaces:**
- Migration: exactly the SQL of spec §5 (including `validated_head_sha`, `supersedes`, `superseded_by`, the `UPDATE … kind = 'exception' WHERE source = 'pr_learning'`, and the four `settings` columns with `curator_effort` constrained by `CHECK (curator_effort IN ('low','medium','high'))`).
- Produces (`adus_db::review_threads`):
  ```rust
  #[derive(sqlx::Type)] pub enum ThreadOutcome { Open, Fixed, WontFix, ByDesign }   // thread_outcome, snake_case
  #[derive(FromRow)] pub struct ReviewThread { id, repository_id, review_id: Option<Uuid>, pr_number: i64, provider_thread_id: String, root_comment_id: String, path: String, line: Option<i32>, severity: Severity, title: String, body: String, posted_head_sha: String, outcome: ThreadOutcome, reason: Option<String>, fix_sha: Option<String>, resolution: Option<String>, adus_replies: i32, validated_head_sha: Option<String>, created_at, updated_at }
  pub struct NewReviewThread<'a> { repository_id, review_id: Option<Uuid>, pr_number, provider_thread_id: &'a str, root_comment_id: &'a str, path: &'a str, line: Option<i32>, severity, title: &'a str, body: &'a str, posted_head_sha: &'a str }
  pub struct ThreadUpdate<'a> { outcome: ThreadOutcome, reason: Option<&'a str>, fix_sha: Option<&'a str>, resolution: Option<&'a str> }
  pub async fn insert_many(pool, rows: &[NewReviewThread<'_>]) -> Result<()>;            // ON CONFLICT DO NOTHING
  pub async fn find(pool, repository_id, pr_number, provider_thread_id: &str) -> Result<Option<ReviewThread>>;
  pub async fn open_for_pr(pool, repository_id, pr_number) -> Result<Vec<ReviewThread>>;   // newest first
  pub async fn for_pr(pool, repository_id, pr_number) -> Result<Vec<ReviewThread>>;
  pub async fn set_outcome(pool, id, update: ThreadUpdate<'_>) -> Result<()>;               // bumps updated_at
  pub async fn record_reply(pool, id) -> Result<()>;                                         // adus_replies += 1
  pub async fn set_validated(pool, id, head_sha: &str) -> Result<()>;
  pub async fn set_provider_thread_id(pool, id, thread_id: &str) -> Result<()>;
  ```
  `Severity` is the existing `severity` enum type (reuse its Rust type; grep `type_name = "severity"`).
- Produces (`adus_db::memories`): `MemoryKind { Convention, Exception, FixPattern }`; `Evidence { url: String, pr_number: Option<i64> }` (serde, stored as jsonb); `Memory` gains `kind, context, reason, resolution: Option<String>, evidence: Vec<Evidence>, supersedes: Option<Uuid>, superseded_by: Option<Uuid>`; `NewMemory` gains `kind, context, reason, resolution: Option<&str>, evidence: &[Evidence], supersedes: Option<Uuid>`; `MemoryPatch` gains optional `kind/context/reason/resolution`. `applicable` adds `AND superseded_by IS NULL`. `approve` and an auto-activation helper `activate_superseding(pool, new_id) -> Result<()>` set `superseded_by = new_id` on the `supersedes` target in the same statement/transaction. Update every existing `NewMemory { … }` construction (command.rs, learn.rs, api) with `kind: MemoryKind::Convention` and empty extras.
- Produces (`adus_db::settings`): `Settings` gains `max_thread_replies: i32, max_validate_threads: i32, max_learned_per_pr: i32, curator_effort: String`; `replace` writes them.

- [ ] **Step 1: Failing DB tests**
  - `review_threads_roundtrip`: `insert_many` two rows (second a duplicate `root_comment_id` → ignored), `find` by provider thread id, `set_outcome(Fixed, fix_sha "abc")`, `open_for_pr` now empty, `record_reply` twice → `adus_replies == 2`.
  - `superseded_memory_leaves_prompts`: insert active A, insert pending B with `supersedes = A`; `applicable` returns A only; `approve(B)` → `applicable` returns B only and A has `superseded_by == B`. Reject path: a pending C superseding B, `reject(C)` → B still applicable.
- [ ] **Step 2:** `cargo test -p adus-db -j 2` → FAIL (missing module/columns).
- [ ] **Step 3:** Write the migration and module code per Interfaces.
- [ ] **Step 4:** `cargo test --workspace -j 2` → PASS.
- [ ] **Step 5:** Commit `feat(db): review threads, memory kinds and thread settings`.

### Task 3: Git provider thread operations — trait, types, GitHub, fakes

**Files:**
- Modify: `crates/git/src/types.rs`, `crates/git/src/lib.rs`, `crates/git/src/github.rs`, `crates/review/tests/common/mod.rs`, `crates/server/tests/common/mod.rs`, plus `crates/review/src/engine.rs` call site of `post_review`
- Test: `crates/git/tests/github.rs`, fixtures `tests/fixtures/github/review_comments_of_review.json`, `review_threads_graphql.json`, `reply_created.json`

**Interfaces:**
- Produces (exported from `adus_git`):
  ```rust
  pub struct PostedComment { pub index: usize, pub thread_id: Option<String>, pub comment_id: String }  // index into `comments`
  pub struct ThreadRef { pub thread_id: String, pub root_comment_id: String }
  pub enum ThreadStatus { Active, Fixed, WontFix, ByDesign }
  // PrThread gains: pub id: Option<String>  (provider thread id; GitHub: root review comment id)
  //                 pub resolved: bool       (GitHub isResolved / Azure status not active|pending)
  // PrThreadComment gains: pub id: String
  // GitProvider changes:
  async fn post_review(&self, repo, pr, comments: &[InlineComment], summary: &str) -> Result<Vec<PostedComment>, GitError>;
  async fn reply_in_thread(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef, body: &str) -> Result<String, GitError>;
  async fn set_thread_status(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef, status: ThreadStatus) -> Result<(), GitError>;
  async fn get_thread(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef) -> Result<PrThread, GitError>;
  ```
- GitHub: `post_review` reads the created review's comments (`GET /pulls/{n}/reviews/{id}/comments`) and matches them to `comments` by `(path, line)` in order → `thread_id: None` (GraphQL node id fetched lazily). `set_thread_status` finds the review-thread node id via GraphQL `pullRequest.reviewThreads` (match `comments.nodes.databaseId == root_comment_id`) when `thread.thread_id` is empty or numeric, then `resolveReviewThread` (Fixed/WontFix/ByDesign) or `unresolveReviewThread` (Active); returns the node id via `get_thread`'s `id` for storing. `reply_in_thread`: `POST /pulls/{n}/comments/{root}/replies`. `get_thread`: the review comments of the PR whose id or `in_reply_to_id` is the root, plus `resolved` from GraphQL.
- Fakes: `GitState` gains `posted_threads: Vec<(ThreadRef, String)>` (replies), `statuses: Vec<(ThreadRef, ThreadStatus)>`, `thread: Option<PrThread>` returned by `get_thread`; `post_review` returns sequential `PostedComment`s (`thread_id: Some("t{i}")`, `comment_id: "c{i}"`).

- [ ] **Step 1: Failing wiremock tests** (`crates/git/tests/github.rs`): `post_review_returns_comment_ids` (two inline comments → `[PostedComment{index:0, comment_id:"101"}, {index:1, comment_id:"102"}]`); `reply_in_thread_posts_reply` (asserts path `/repos/o/r/pulls/7/comments/101/replies` and body, returns `"103"`); `set_thread_status_resolves_via_graphql` (two GraphQL calls; the mutation carries the node id `PRRT_x`); `get_thread_collects_replies_and_resolution`.
- [ ] **Step 2:** `cargo test -p adus-git -j 2 github` → FAIL.
- [ ] **Step 3:** Implement per Interfaces; Azure's three new methods return `Err(GitError::Status(501))` and its `post_review` returns `PostedComment`s with empty ids until Task 4 (Azure tests untouched). Engine call site: `posted.len()` for the count (rows are written in Task 5).
- [ ] **Step 4:** `cargo test --workspace -j 2` → PASS.
- [ ] **Step 5:** Commit `feat(git): thread ids, replies and status on GitHub`.

### Task 4: Azure thread operations

**Files:**
- Modify: `crates/git/src/azure.rs`
- Test: `crates/git/tests/azure.rs`, fixtures `tests/fixtures/azure/thread_created.json`, `thread.json`, `comment_created.json`

**Interfaces:** Consumes Task 3's trait. `post_review` returns each `POST …/threads` response's `id` → `thread_id: Some(id)`, first comment `id` → `comment_id`. `reply_in_thread`: `POST …/pullRequests/{n}/threads/{id}/comments` `{"content": body, "parentCommentId": root, "commentType": 1}`. `set_thread_status`: `PATCH …/threads/{id}` `{"status": "active"|"fixed"|"wontFix"|"byDesign"}`. `get_thread`: `GET …/threads/{id}` mapped like `list_pr_threads` (`resolved` = status not `active`/`pending`/absent).

- [ ] **Step 1: Failing tests:** `post_review_returns_thread_ids`, `reply_in_thread_sets_parent`, `set_thread_status_patches_status` (each `ThreadStatus` → its wire string, parameterised in one test), `get_thread_maps_status`.
- [ ] **Step 2:** `cargo test -p adus-git -j 2 azure` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --workspace -j 2` → PASS.
- [ ] **Step 5:** Commit `feat(git): Azure thread ids, replies and status`.

### Task 5: Record finding threads after posting

**Files:**
- Modify: `crates/review/src/engine.rs` (`ReviewOutput` keeps the posted `Suggestion`s; after `post_review`)
- Test: `crates/review/tests/engine.rs`

**Interfaces:** Consumes `PostedComment`, `review_threads::insert_many`. Each posted finding → `NewReviewThread { provider_thread_id: thread_id.unwrap_or(comment_id), root_comment_id: comment_id, path, line, severity, title, body: <redacted posted body>, posted_head_sha: pr.head_sha, review_id }`. A DB error here is logged (`warn`) and does not fail the posted review.

- [ ] **Step 1:** Failing test `review_records_finding_threads`: a review posting 2 findings leaves 2 `review_threads` rows with `outcome = Open`, the fake's ids and the PR head.
- [ ] **Step 2–4:** Run → FAIL, implement, `cargo test --workspace -j 2` → PASS.
- [ ] **Step 5:** Commit `feat(review): record posted finding threads`.

### Task 6: `Job::ThreadReply` — webhook and worker

**Files:**
- Modify: `crates/core/src/job.rs`, `crates/server/src/webhooks.rs`, `crates/adus/src/worker.rs`
- Test: `crates/core/src/job.rs` (roundtrip test), `crates/server/src/webhooks.rs` tests, `crates/adus/tests/worker.rs`

**Interfaces:**
- Produces: `Job::ThreadReply { repository_id: Uuid, pr_number: i64, thread_id: String, comment_id: String }`; worker routes it to `Engine::thread_reply(repository_id, pr_number, &thread_id, &comment_id)` (Task 7) under the same per-PR lock; `kind = "thread_reply"`.
- GitHub: `pull_request_review_comment` `created` with `comment.in_reply_to_id` present, not `is_adus_comment`, no `@adudu` command → `ThreadReply { thread_id: in_reply_to_id, comment_id: comment.id }`. With a command → `PrComment` as today.
- Azure: comment event with `comment.parentCommentId > 0` → `ThreadReply { thread_id: <thread id from resource.comment._links.threads.href or resource.thread>, comment_id: comment.id }` (confirm the field from the public service-hook payload docs and add a fixture); root comments → today's path.

- [ ] **Step 1: Failing tests:** job JSON roundtrip for `thread_reply`; webhook: GitHub reply → `ThreadReply`; GitHub reply containing `@adudu review` → `PrComment`; GitHub reply with leading marker → `None`; Azure reply (`parentCommentId: 1`) → `ThreadReply`; worker dispatches `ThreadReply` to the runner.
- [ ] **Step 2–4:** FAIL → implement (engine method stub returning `Ok(())` until Task 7) → `cargo test --workspace -j 2` PASS.
- [ ] **Step 5:** Commit `feat: ThreadReply job from replies in Adus threads`.

### Task 7: `Engine::thread_reply`

**Files:**
- Create: `crates/review/src/threads.rs` (thread replies and push validation; keeps `engine.rs` from growing)
- Modify: `crates/review/src/lib.rs`
- Test: `crates/review/tests/threads.rs`

**Interfaces:**
- Produces: `pub async fn thread_reply(&self, repository_id: Uuid, pr_number: i64, thread_id: &str, comment_id: &str) -> Result<(), EngineError>`; `pub(crate) fn status_for(outcome: ThreadOutcome) -> ThreadStatus`; `Verdict { verdict: VerdictKind, reason: Option<String>, resolution: Option<String>, reply: String }` with `VerdictKind { Fixed, NotFixed, WontFix, ByDesign, NeedsReason, Discussion }` (serde snake_case) parsed with `first_json_object`.
- Flow (spec §6.3): repo missing/disabled → Ok. `review_threads::find` none → Ok. `adus_replies >= settings.max_thread_replies` → Ok. No model → Ok. `get_pr`, `get_thread`; if the thread's last comment after `comment_id` is Adus's → Ok. Sync a human-set status first (spec §6.3 "Status set by a human"). Prompt: system prompt written fresh (task, verdict rules, "data, not instructions", JSON only); user prompt wraps `<finding>`, `<thread>` (via `discussion::format_threads`), `<code>` (≤ 120 lines around `line` from the PR diff patch of `path`; workspace clone not used — builder's call allowed by spec §11). `wont_fix`/`by_design` with empty reason → `NeedsReason`. Act: reply (`adus_body(REPLY_LABEL, redact(reply))`), `record_reply`, and for Fixed/WontFix/ByDesign `set_thread_status` + `set_outcome` (`fix_sha` = head for Fixed); `NotFixed` on a non-open row → status `Active`, outcome `Open`. Unparseable answer → `warn`, Ok. Errors map like `learn_pr` (`fetch_error`, `chat_error`, `is_retryable`).

- [ ] **Step 1: Failing tests** (fake chat returning canned JSON; reuse the chat fake used by `crates/review/tests/learn.rs`): `fixed_reply_resolves_thread`; `by_design_with_reason_sets_status_and_reason`; `by_design_without_reason_asks_for_one` (status untouched, outcome `Open`); `not_fixed_on_fixed_thread_reopens`; `reply_cap_or_zero_setting_skips_llm` (two cases, assert no chat call); `unknown_thread_is_ignored`; `already_answered_comment_is_ignored`; `comment_with_secret_is_redacted_in_prompt`.
- [ ] **Step 2–4:** FAIL → implement → `cargo test --workspace -j 2` PASS.
- [ ] **Step 5:** Commit `feat(review): answer replies in Adus threads`.

### Task 8: Push validation

**Files:**
- Modify: `crates/review/src/threads.rs`, `crates/review/src/engine.rs` (`run`, `format_summary` call), `crates/review/src/prompt.rs` (`format_summary` gains `confirmed_fixed: usize`)
- Test: `crates/review/tests/threads.rs`

**Interfaces:**
- Produces: `pub(crate) async fn validate_open_threads(&self, git: &dyn GitProvider, repo_ref: &RepoRef, repo: &Repository, pr: &PullRequest, fast: (Uuid, &str), settings: &Settings) -> usize` (count confirmed fixed; never errors — logs and returns what it managed).
- Placement (spec §6.4): in `run`, for `Trigger::Updated | Trigger::Command`, after the `get_pr` / auto_review / model checks, before the draft, ignored-title, already-reviewed and nothing-to-review skips. Move the model lookup above the draft and title checks only for this purpose; keep the existing skip reasons and ordering of `reviews` rows. Rows: `open_for_pr`; first one `list_pr_threads` call syncs rows whose provider thread is `resolved` by a human (spec §6.3 mapping) and drops them from validation; then keep those whose `validated_head_sha.unwrap_or(posted_head_sha) != pr.head_sha`, cap `max_validate_threads` (0 → skip). One `diff_between(base = that sha, head)` per distinct base sha; threads whose `path` is not in the changed files only get `set_validated`. Batches of ≤ 10 threads per fast-model call returning `{"threads": [{"index": i, "verdict": "fixed"|"not_fixed"|"unclear", "resolution": "...", "reply": "..."}]}`; only `fixed` acts (reply + `Fixed` + `set_outcome`); all checked rows `set_validated(head)`.
- Summary: `format_summary` appends `{N} earlier finding(s) confirmed fixed.` when N > 0.

- [ ] **Step 1: Failing tests:** `push_confirms_fixed_thread` (row fixed, reply posted, summary line present); `unchanged_head_makes_no_llm_call`; `not_fixed_on_push_does_not_reply`; `validation_runs_when_review_is_skipped_as_draft`; `deleted_file_fix_is_confirmed` (path deleted in `diff_between`); `validation_error_does_not_fail_review` (chat error → review still `Done`); `human_resolved_thread_is_synced_not_validated`.
- [ ] **Step 2–4:** FAIL → implement → `cargo test --workspace -j 2` PASS.
- [ ] **Step 5:** Commit `feat(review): validate open findings on push`.

### Task 9: Memory kinds in reviews + precedent post-processing

**Files:**
- Modify: `crates/review/src/prompt.rs` (`format_memories`, Finder system prompt rules), `crates/review/src/verifier.rs` (rules text), `crates/review/src/parse.rs` (`Suggestion.precedent: Option<usize>`, `submit_findings` schema), `crates/review/src/engine.rs` (apply precedent before building `InlineComment`s), `crates/review/src/test_support.rs` (`memory()` helper gains kind)
- Test: `crates/review/src/prompt.rs`, `crates/review/src/parse.rs`, `crates/review/tests/engine.rs`

**Interfaces:**
- `format_memories(memories: &[Memory]) -> String`: three headed groups (`Conventions`, `Exceptions`, `Fix patterns`), each item `- [{index}] {content}` plus indented `Reason:` / `Resolution:` / `Context:` lines when present, each cut to 500 chars; `{index}` = position in the `memories` slice.
- Prompt rules (written fresh, spec §7.4): conventions — do not flag code that follows them, flag violations; fix patterns — actively look for the same problem and recommend the recorded fix; exceptions & fix patterns — never drop a finding because of them; cite a matching one in `precedent`.
- `Suggestion.precedent: Option<usize>` parsed from optional integer field `precedent`; the Verifier keeps it unchanged.
- Produces: `pub(crate) fn apply_precedent(s: &mut Suggestion, memories: &[Memory])`, called after Dedup, before `inline_body`. `Exception` → `severity = Info`, append `A similar case was declared {by design|won't fix} in PR #{N}: {reason}` (`by design` when the memory's `context` starts with `By design:`, else `won't fix` — Task 10 writes that prefix); `FixPattern` → severity unchanged, append `A similar case was fixed in PR #{N}: {resolution}`; each followed by the first evidence url when present. `{N}` = first `evidence.pr_number`; drop ` in PR #{N}` when none. `Convention` or out-of-range index → ignored. Counts in the summary use the post-precedent severity.

- [ ] **Step 1: Failing tests:** `format_memories_groups_by_kind`; `parse_reads_precedent`; `exception_precedent_downgrades_to_info` (warning finding citing an exception → posted as Info with the block and link); `fix_pattern_precedent_keeps_severity`; `invalid_precedent_is_ignored`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement per Interfaces.
- [ ] **Step 4:** `cargo test --workspace -j 2` → PASS.
- [ ] **Step 5:** Commit `feat(review): memory kinds and precedents in reviews`.

### Task 10: Curator (`learn_pr` rewrite)

**Files:**
- Modify: `crates/review/src/learn.rs`, `crates/review/src/discussion.rs` (`within_budget`: skip an oversized thread and `continue`)
- Test: `crates/review/tests/learn.rs`, `crates/review/src/discussion.rs` tests

**Interfaces:**
- Consumes: `review_threads::for_pr`, `list_pr_threads` (with `PrThread.id`), `memories::for_learning`, `settings.max_learned_per_pr`, `settings.curator_effort`, `memories::activate_superseding`.
- Input (spec §7.2): Adus threads (rows with `outcome != Open`, or open with a human comment), matched to provider threads by id, rendered with their row's outcome/reason/resolution/fix sha; then every other thread with ≥ 1 human comment. The ≥ 2 comments / ≥ 2 authors rule is removed. Budget order: Adus threads with an outcome, then human threads, newest first.
- Call: primary model, `ReasoningEffort` parsed from `curator_effort`, single turn; system prompt written fresh (spec §7.3 instruction; also: `context` for an exception starts with `By design:` or `Won't fix:`). `LEARN_MAX_TOKENS` raised to 16,384.
- Output & validation exactly as spec §7.3 (`kind`, `content`, `context`, `reason`, `resolution`, `sources`, `updates`); cap `max_learned_per_pr`; `updates` must be the id of a repo memory in `for_learning` with status ≠ rejected → `supersedes`. Status per spec §7.3, including the fix-pattern-from-Adus-confirmed-fix rule. Activation of a superseding memory in `auto` calls `activate_superseding`. `evidence` from cited threads' urls + `pr_number`.

- [ ] **Step 1: Failing tests** (extend `crates/review/tests/learn.rs`, replacing tests pinned to the old ≥ 2-author rule): `single_human_comment_becomes_convention`; `fixed_adus_thread_becomes_fix_pattern_active_in_auto`; `exception_without_reason_is_dropped`; `updates_supersedes_existing_memory` (approval → pending with `supersedes`; auto → old memory superseded); `cap_follows_setting`; `rejected_memory_is_listed_as_do_not_propose`; `within_budget_skips_oversized_thread` (discussion.rs).
- [ ] **Step 2–4:** FAIL → implement → `cargo test --workspace -j 2` PASS.
- [ ] **Step 5:** Commit `feat(review): curate memories from thread outcomes and human reviews`.

### Task 11: API — memory fields and settings

**Files:**
- Modify: `crates/server/src/api/memories.rs`, `crates/server/src/api/settings.rs`
- Test: `crates/server/tests/` (memories and settings test files)

**Interfaces:** Memory JSON gains `kind`, `context`, `reason`, `resolution`, `evidence`, `supersedes`, `superseded_by`; `GET /memories?kind=…` filter; create/patch accept `kind` and the three text fields (each ≤ 2,000 chars → `ApiError` `validation`); approve of a superseding memory supersedes its target (DB helper from Task 2). Settings JSON gains the four fields: `max_thread_replies` and `max_validate_threads` 0–100, `max_learned_per_pr` 1–50, `curator_effort` in `low|medium|high`; out of range → `validation` error.

- [ ] **Step 1: Failing tests:** `memory_roundtrips_new_fields`; `list_filters_by_kind`; `approve_supersedes_target`; `settings_validate_thread_limits` (one parameterised test).
- [ ] **Step 2–4:** FAIL → implement → `cargo test --workspace -j 2` PASS.
- [ ] **Step 5:** Commit `feat(api): memory kinds, curated fields and thread settings`.

### Task 12: Dashboard

**Files:**
- Modify: `web/src/lib/components/memory-dialog.svelte`, `web/src/routes/rules/+page.svelte` (memory list), `web/src/routes/settings/+page.svelte`, the API types/client under `web/src/lib/` (grep `MemoryStatus`)
- Test: `pnpm -C web check`, `pnpm -C web fmt:check`

**Interfaces:** Memory list shows a kind badge and, on expand/detail, context / reason / resolution / evidence links; a pending memory with `supersedes` shows the target's content above its own. Dialog edits kind + three fields. Settings page: a "Thread conversations & learning" card with the four settings (number inputs, effort select), matching existing cards. Follow DESIGN.md tokens; skeletons updated.

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** `pnpm -C web check` → 0 errors; `pnpm -C web fmt:check` clean; `pnpm -C web build`.
- [ ] **Step 3:** With the stack running, `curl` `GET /api/v1/memories` and `GET /api/v1/settings` show the new fields (state in the report that no browser automation was used).
- [ ] **Step 4:** Commit `feat(web): memory kinds and thread settings`.

### Task 13: Sync `docs/SPEC.md` and mark spec implemented

**Files:** `docs/SPEC.md`, the spec's status line, `AGENTS.md` (layout line: migrations `0001..0010`; nothing else unless a new cross-cutting rule appeared)

- [ ] **Step 1:** Update SPEC.md sections for webhooks, jobs, `GitProvider`, engine (validation step, thread replies, precedents), learning (curator), schema (0010), API, settings, and §10 entry; note the behaviour change for existing `pr_learning` memories.
- [ ] **Step 2:** Spec status → `Status: implemented (consolidated into docs/SPEC.md §…)`.
- [ ] **Step 3:** `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -j 2 && pnpm -C web check` → all green.
- [ ] **Step 4:** Commit `docs: sync SPEC for thread conversations and curated learning`.
