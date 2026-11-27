# Bot-Labelled Comments, Thread Conversations & Curated Learning — Design Specification

Date: 2026-11-27 · Status: approved (2026-11-27), not implemented

> **Binding scope & policy:** Extends [docs/SPEC.md](../../SPEC.md), the agentic review engine spec ([2026-11-21-agentic-review-engine-design.md](2026-11-21-agentic-review-engine-design.md)) and the PR discussion spec ([2026-11-23-pr-discussion-awareness-and-learning-design.md](2026-11-23-pr-discussion-awareness-and-learning-design.md)). Where this spec contradicts the 2026-11-23 spec (silent learning, no replies, memories suppress findings), this spec wins. The clean-room and no-assumption rule in `AGENTS.md` applies: Kodus community code may be read as a behaviour reference only (cite it), Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened. All prompts, schemas and code are written fresh for Adus.

---

## 1. Intent & Why

Adus posts with a **personal PAT**, so on the PR every Adus comment shows under a real person's account. Three problems follow:

1. **People cannot tell Adus from the PAT owner.** Inline findings render as `**⚠️ Warning:** title` with nothing saying a bot wrote them; only the summary says "Adudu". Adus itself already recognises its comments through the hidden `<!-- adudu -->` marker, but humans cannot see it.
2. **Findings are fire-and-forget.** After Adus posts a finding, the developer either (a) pushes a fix, or (b) replies that it is *won't fix* or *by design* for some reason. Adus never answers, never checks the fix, never changes the thread status. Threads stay open forever or are closed by hand without Adus learning anything.
3. **Learning almost never fires.** Today learning runs only on PR close, only extracts "by design" exceptions, and only from threads with ≥ 2 comments from ≥ 2 authors. Most Adus findings get no reply (the developer just fixes or ignores them), so the outcome — the most common signal — is thrown away. What is stored is a one-line "expected" statement without the context that made it true.

**Success:**

- Every Adus comment is visibly labelled as the bot; Adus never mistakes a human reply (even from the PAT owner's account) for its own.
- In Adus's own finding threads, Adus answers every human reply, validates claimed and pushed fixes against the code, and sets the thread status itself.
- Every thread outcome (fixed / won't fix / by design, with reason and fix) and every convention a human reviewer states is curated into rich, reusable memories, which shape later reviews: similar cases are flagged with the precedent and its recommended fix, or downgraded to Info with the recorded reason.

## 2. Goals, Non-Goals, Anti-Goals

### Goals

1. **Bot label (Part 1).** Every comment Adus posts (inline finding, summary, command reply, thread reply) starts with a visible bot label. The hidden marker stays and stays the machine signal.
2. **Reliable self-recognition (Part 1).** A comment counts as Adus's only when its body **starts with** the marker (after leading whitespace), and Adus records the provider ids of every finding thread it creates (§5). A human quoting an Adus comment is never treated as Adus.
3. **Thread conversations (Part 2).** Adus replies in its **own finding threads**, to **every comment from a real user** (no marker), without needing `@adudu`.
4. **Fix validation (Part 2).** On each new push, Adus re-checks its open finding threads on changed files; on a reply claiming a fix, it checks the current head. Fixed → reply + resolve. Not (fully) fixed → reply saying what is left, thread stays open.
5. **Won't fix / by design (Part 2).** On a reply giving such a reason, Adus confirms what it understood and sets the status (Azure `wontFix` / `byDesign`; GitHub resolves the thread). Without a reason, Adus asks for one and leaves the thread open.
6. **Outcome record (Part 2).** Every thread decision is stored immediately in `review_threads` (outcome, reason, fix commit, discussion summary), independent of learning mode, so nothing is lost if the PR closes without further discussion.
7. **Curated learning (Part 3).** On PR close, one curator call reads the PR's `review_threads` outcomes **and** human reviewer threads (not Adus's) and writes memories of three kinds — **convention**, **exception**, **fix pattern** — each a general, reusable statement plus the curated context: what the pattern is, a summary of the discussion, the reason, the resolution / recommended fix, and evidence links. Similar existing memories are updated rather than duplicated.
8. **Memory-aware reviews (Part 3).** In later reviews:
   - **convention** → code that follows it is not flagged; code that violates it is (as today's memory/rule semantics);
   - **exception** (by design / won't fix) → a matching finding is **still posted, downgraded to ℹ️ Info**, with "a similar case was declared by design / won't fix in PR #N because …" and the link;
   - **fix pattern** → a matching finding is posted as usual with the precedent and its recommended fix.
9. Learning mode `off` / `approval` / `auto` per repo keeps working as today for curated memories.

### Non-Goals (this wave)

- Replying to threads Adus did not open, or to the summary thread (only `@adudu` commands are answered there, as today).
- A dashboard page listing `review_threads` (data is recorded for a later page; only memories get UI changes).
- Learning from PR descriptions or commit messages beyond the fix commit of a validated thread.
- Global-scope learned memories (repo scope only, as today).
- Re-opening a thread Adus resolved when later code regresses (a new finding is posted instead by the normal review).

### Anti-Goals

- Adus replying to itself or looping (bot ↔ bot, or reply storms on a busy thread).
- A developer closing a real bug just by writing "by design": the thread may be marked `byDesign`, but the curated memory obeys the learning mode (`approval` → `pending`; `auto` → the registered-defender rule of 2026-11-23 §5.3), and an exception never **suppresses** a later finding — it only downgrades it to Info with the reason visible.
- Prompt injection from thread comments changing Adus's verdict format, statuses on other threads, or memories.
- Secrets from comments or code reaching memories or logs (all comment bodies go through `redact_secrets`; bodies are never logged).
- Thin memories: a learned memory that is only an "expected" one-liner without reason and context.

## 3. Approaches Considered

| | Approach | Trade-off |
|---|---|---|
| **A (chosen)** | **Record Adus's finding threads in a DB table** (`review_threads`): provider thread/comment ids, finding, status, outcome, reason, fix commit | Reliable (survives edited/deleted comments), outcomes stored the moment they happen, data usable for a later dashboard; costs one migration and more code |
| B | Stateless: finding metadata in the hidden marker as JSON, everything re-read from provider threads each time | No thread table, but brittle (edited comments, marker size), outcomes only captured at PR close, no history |
| C | Store each outcome directly as a memory | Rejected: the user wants outcomes curated, not dumped |

The user picked **A** (2026-11-27). Decisions from the conversation (2026-11-27):

- Adus replies only in threads Adus created, only to comments from real users (no marker).
- Pattern seen again later: by design / won't fix → still flagged, downgraded to Info with context; fixed → flagged with the precedent's recommended fix.
- Human reviewer threads (not Adus's) are also learned from.
- Fix patterns are actively looked for in later reviews, not only used to label findings raised anyway.
- Limits are dashboard settings (§9), global, no per-repo override.
- Memories carry the curated discussion (pattern, context, reason, resolution, evidence), not just the expected statement.

## 4. Part 1 — Bot Label & Self-Recognition

### 4.1 Label

- Every Adus comment body is `MARKER + "\n" + LABEL + …`. `LABEL` is a short visible line, e.g. `🤖 **Adudu**` followed on the same line by what the comment is:
  - inline finding: `🤖 **Adudu** · ⚠️ **Warning:** <title>`
  - summary: `🤖 **Adudu** reviewed this PR` (replaces `### Adudu reviewed this PR`)
  - command reply / thread reply: `🤖 **Adudu**` on its own line, then the text.
- One helper in `crates/review/src/prompt.rs` builds every body so the marker-then-label order is enforced in one place; `format_inline`, `format_summary`, the command reply and the no-model notice use it.

### 4.2 Recognition

- `by_adus` (GitHub and Azure `list_pr_threads`), the webhook filters, and `parse_command` use one shared predicate in `crates/core` — `is_adus_comment(body)` = trimmed body starts with `ADUDU_MARKER` — instead of `contains`. A human reply quoting an Adus comment (marker mid-body) is a human comment.
- The PAT owner's own replies carry no marker and are human comments, though posted from the same account.

## 5. Data Model (migration `0010`)

```sql
CREATE TYPE thread_outcome AS ENUM ('open', 'fixed', 'wont_fix', 'by_design');
CREATE TYPE memory_kind AS ENUM ('convention', 'exception', 'fix_pattern');

-- One row per finding thread Adus opened.
CREATE TABLE review_threads (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id      uuid NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    review_id          uuid REFERENCES reviews (id) ON DELETE SET NULL,
    pr_number          bigint NOT NULL,
    provider_thread_id text NOT NULL,   -- Azure thread id / GitHub root review comment id (GraphQL node id is looked up, never stored)
    root_comment_id    text NOT NULL,   -- Azure comment id within the thread / GitHub review comment id
    path               text NOT NULL,
    line               integer,
    severity           severity NOT NULL,
    title              text NOT NULL,
    body               text NOT NULL,   -- finding body as posted (redacted)
    posted_head_sha    text NOT NULL,
    outcome            thread_outcome NOT NULL DEFAULT 'open',
    reason             text,            -- developer's stated reason (won't fix / by design)
    fix_sha            text,            -- head where Adus confirmed the fix
    resolution         text,            -- how it was fixed / what is left, Adus's words
    adus_replies       integer NOT NULL DEFAULT 0,
    validated_head_sha text,            -- last head push validation checked this thread at
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repository_id, pr_number, provider_thread_id)
);
CREATE INDEX review_threads_open_idx ON review_threads (repository_id, pr_number) WHERE outcome = 'open';

ALTER TABLE memories
    ADD COLUMN kind          memory_kind NOT NULL DEFAULT 'convention',
    ADD COLUMN context       text,        -- curated summary of the discussion
    ADD COLUMN reason        text,
    ADD COLUMN resolution    text,        -- fix applied / recommended fix
    ADD COLUMN evidence      jsonb NOT NULL DEFAULT '[]',  -- [{"url": "...", "pr_number": n}]
    ADD COLUMN supersedes    uuid REFERENCES memories (id) ON DELETE SET NULL,
    ADD COLUMN superseded_by uuid REFERENCES memories (id) ON DELETE SET NULL;

-- Learned memories from the 2026-11-23 wave are by-design exceptions.
UPDATE memories SET kind = 'exception' WHERE source = 'pr_learning';

ALTER TABLE settings
    ADD COLUMN max_thread_replies   integer NOT NULL DEFAULT 5,   -- 0 = Adus never replies in threads
    ADD COLUMN max_validate_threads integer NOT NULL DEFAULT 30,  -- 0 = no validation on push
    ADD COLUMN max_learned_per_pr   integer NOT NULL DEFAULT 10 CHECK (max_learned_per_pr >= 1),
    ADD COLUMN curator_effort       text    NOT NULL DEFAULT 'medium'; -- low | medium | high
```

- Existing memories become `convention` with empty extras; `content` stays the general statement (≤ 2,000 chars). Each extra text field ≤ 2,000 chars, `evidence` ≤ 10 entries, http(s) only.
- `supersedes` (on the new, possibly pending memory) and `superseded_by` (set on the old one at activation) implement "update instead of duplicate" (§7.3). *(Builder check 2026-11-27, my call: a pending memory needs somewhere to store its target.)*
- Existing `pr_learning` memories become `exception` *(my call, consistent with the user's decision that by-design cases are downgraded, not suppressed)*: from release on they no longer suppress findings; their Precedent block shows `content` and the `source_url` when there is one.
- The four `settings` columns are edited on the dashboard Settings page next to `max_files_per_review` (§9). No per-repo override this wave.

## 6. Part 2 — Thread Conversations

### 6.1 Recording threads

- `GitProvider::post_review` returns, per posted inline comment, its provider ids (`PostedComment { index, thread_id, comment_id }`) instead of a count. Azure: the `POST …/threads` response has the thread id and its first comment id. GitHub: after `POST /pulls/{n}/reviews`, read the review's comments (`GET /pulls/{n}/reviews/{review_id}/comments`) and match them to the inline list; the GraphQL review-thread node id (needed to resolve) is looked up when first needed and stored.
- The engine inserts one `review_threads` row per posted finding, in the same step that records the review.

### 6.2 New provider operations (`crates/git`, both providers + `FakeGit`)

```rust
/// Replies inside a thread. Returns the new comment id.
async fn reply_in_thread(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef, body: &str) -> Result<String, GitError>;
/// Sets the thread status. GitHub has only resolved/unresolved: Fixed, WontFix and ByDesign all resolve.
async fn set_thread_status(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef, status: ThreadStatus) -> Result<(), GitError>;
/// The current comments of one thread, oldest first (same shape as `PrThread`).
async fn get_thread(&self, repo: &RepoRef, pr_number: i64, thread: &ThreadRef) -> Result<PrThread, GitError>;
```

- `ThreadRef { thread_id, root_comment_id }`; `ThreadStatus { Active, Fixed, WontFix, ByDesign }`.
- Azure: `POST …/threads/{id}/comments` (`parentCommentId` = root), `PATCH …/threads/{id}` `{"status": "fixed"|"wontFix"|"byDesign"|"active"}`, `GET …/threads/{id}`.
- GitHub: `POST /pulls/{n}/comments/{root}/replies`; resolve via GraphQL `resolveReviewThread` (unresolve via `unresolveReviewThread`); thread comments via `GET /pulls/{n}/comments` filtered by `in_reply_to_id`. The builder confirms exact wire formats from public provider docs and records them in fixtures.
- Exact endpoints, api-versions and error mapping follow the existing `send` / `decode` conventions.

### 6.3 Trigger 1: a human reply in an Adus thread

- **Webhook:** GitHub `pull_request_review_comment` (`created`, has `in_reply_to_id`); Azure `ms.vss-code.git-pullrequest-comment-event` with a non-root comment. If the comment is from Adus (§4.2), ignore. If it contains an `@adudu` command, the command path wins (today's behaviour). Otherwise enqueue `Job::ThreadReply { repository_id, pr_number, thread_id, comment_id, author }`; the engine decides whether the thread is Adus's (row in `review_threads`) — not the webhook, which has no DB access beyond the repo lookup it does today.
- **Engine (`ThreadReply`):** skip silently if no `review_threads` row, the repo is disabled, or the thread's `adus_replies` ≥ `MAX_THREAD_REPLIES` (5). Fetch the thread (`get_thread`) and the PR; one fast-model call (`ReasoningEffort::Low`) with: the finding, the thread so far (redacted, author `adus` for marker comments), and the current code around the finding's path/line at PR head (from a shallow workspace when available, else the PR diff of that file). Output JSON:

  ```json
  {"verdict": "fixed" | "not_fixed" | "wont_fix" | "by_design" | "needs_reason" | "discussion",
   "reason": "<developer's reason, when wont_fix / by_design>",
   "resolution": "<what fixed it / what is left>",
   "reply": "<markdown reply to post>"}
  ```

  - `fixed` → Adus checked the head code; reply, set status `Fixed`, row `outcome=fixed`, `fix_sha`=head, `resolution`.
  - `not_fixed` → reply with what is left; status stays.
  - `wont_fix` / `by_design` → only valid with a non-empty `reason`; reply confirms it, status `WontFix` / `ByDesign`, row updated. Empty reason → treated as `needs_reason`.
  - `needs_reason` → reply asking for the reason; status stays.
  - `discussion` (question, disagreement without a decision) → reply answering it; status stays.
- Reply bodies go through `redact_secrets` and the label helper; `adus_replies` increments per reply. Unparseable model answer → no reply, logged (`warn`, no body), job done (no retry storm). Retryable LLM / git errors retry like other jobs.
- **Any Adus thread, any outcome** *(my call)*: a reply in a thread already `fixed` / `wont_fix` / `by_design` is handled the same way; the verdict may change the outcome, and `not_fixed` on a `fixed` thread sets status `Active` and outcome `open`.
- **Status set by a human** *(my call)*: before acting, the engine reads the thread's provider status (`get_thread`). If a human closed it without a reply (Azure `fixed`/`closed`/`wontFix`/`byDesign`, GitHub resolved), the row is synced (`fixed`/`closed` → `fixed`, `wontFix` → `wont_fix`, `byDesign` → `by_design`, `reason` empty) and push validation leaves it alone. A human-set won't fix / by design without a reason is not learnable as an exception (§7.3 needs a reason).
- **Concurrency with the existing flow.** Every job of one PR (`ReviewPr`, `PrComment`, `ThreadReply`, `LearnPr`) runs under the existing Redis per-PR lock `pr:{repo}:{pr}`; a job that finds the PR busy is retried after 30 s without using an attempt. So jobs never run in parallel on one PR, but their **order is not guaranteed**, and a reply that arrives during a long review waits until the review ends. Every job therefore re-reads its state when it starts (PR head via `get_pr`, the thread via `get_thread`, the `review_threads` row) and never trusts webhook data for the head:
  - reply + push for the same fix: whichever runs second sees the row already `fixed` and the Adus reply already in the thread;
  - a `ThreadReply` job does nothing when the thread's last comment after the triggering comment is already from Adus (no double reply);
  - Adus's own replies and status changes come back as webhooks: comments are dropped by the marker; Azure `git.pullrequest.updated` events from a status change reach `ReviewPr`, where the unchanged head makes both validation and review a no-op.
- Anyone (registered or not) can trigger a thread reply: the developer replying is usually not an Adus user. Cost is bounded by `MAX_THREAD_REPLIES`.

### 6.4 Trigger 2: a new push

- Validation is its own step at the start of a `ReviewPr` run with trigger `updated` or `command`, right after the repo-enabled, `auto_review` (for `updated`) and model checks and **before** the draft / ignored-title / already-reviewed / nothing-to-review skips, so a fix that deletes the file or only touches ignored paths is still confirmed *(builder check, my call)*. It has its own small budget, separate from the review's token budget. Because Azure sends `git.pullrequest.updated` for any change (title, reviewers, votes, Adus's own status changes), validation runs only when the PR head differs from the head the row was last validated at (`review_threads.validated_head_sha`, added to §5), so non-push updates cost no LLM call. The engine validates open `review_threads` rows of this PR whose `path` changed between their `posted_head_sha` (or the last validated head) and the new head (`diff_between`). At most `MAX_VALIDATE_THREADS` (30) per run, newest first.
- One fast-model call per batch of threads (path, finding, the file's current content around the line and the diff since posting) returning per thread `fixed` / `not_fixed` / `unclear` with `resolution` and `reply`. Only `fixed` acts: reply + status `Fixed` + row update. `not_fixed` / `unclear` on a push do **not** reply (avoid noise on every push); they stay open.
- Validation errors never fail the review: logged and skipped.
- Validated-fixed threads count in the summary when a summary is posted (`N earlier finding(s) confirmed fixed.`); otherwise the thread replies are the only record.

## 7. Part 3 — Curated Learning

### 7.1 Trigger

Unchanged: PR close → `Job::LearnPr`, once per PR head (`pr_learning_runs`), skipped when `learning_mode = off`. The "qualifying thread" rule (≥ 2 comments, ≥ 2 authors) is **removed**.

### 7.2 Curator input

- **Adus threads:** every `review_threads` row of the PR with `outcome != open`, plus open ones that have human replies; each with finding, outcome, reason, resolution, fix sha, and the thread comments (re-fetched via `list_pr_threads`, redacted, bounded as today).
- **Human threads:** every non-Adus thread (no `review_threads` row) with at least one human comment — single-comment reviewer remarks count ("use `X`, not `Y` here").
- Existing repo memories (active + pending, with kind and id) and rejected ones (do-not-propose list), bounded as today (20,000 chars each).
- Threads section bounded at 40,000 chars; when over budget, Adus threads with an outcome first, then human threads, newest first. (Fix the known `within_budget` issue: skip an oversized thread and continue, instead of stopping.)

### 7.3 Curator call & output

- One call to the provider's **review model** (not the fast model) at `ReasoningEffort::Medium` — curation quality is the point of this part; it runs once per closed PR.
- Instruction (written fresh): curate every lesson worth keeping; each memory is a **general, reusable statement** naming the pattern (not the PR, file line or people) plus curated context. Skip noise: typos, one-off nits, unresolved debates, cases with no reason or no confirmed fix.
- Output:

  ```json
  {"memories": [{
     "kind": "convention" | "exception" | "fix_pattern",
     "content": "<general statement>",
     "context": "<what happened in the discussion, summarized>",
     "reason": "<why: PRD, team standard, constraint>",
     "resolution": "<the fix applied / recommended fix>",
     "sources": [{"thread": <index>, "comments": [<comment index>, ...]}],
     "updates": "<existing memory id or null>"
  }]}
  ```

- **Validation in code:** valid thread/comment indices; `sources` must cite at least one human comment, or (for `fix_pattern` from an Adus thread) a thread with `outcome = fixed`; `exception` requires a `reason`; `fix_pattern` requires a `resolution`; lengths as §5. Up to `MAX_LEARNED` (10) per PR.
- **Status:** same rules as 2026-11-23 §5.3 (`approval` → `pending`; `auto` → `active` only with a registered defender among the cited human authors and every other human author registered; otherwise `pending`). A `fix_pattern` sourced only from an Adus thread confirmed fixed by Adus is `active` in `auto` mode (the fix is the evidence) and `pending` in `approval`.
- **`updates`:** when set to an existing repo memory id (validated), the new memory is inserted with the merged content and `evidence` = old ∪ new; on activation (immediately in `auto`, on approval in `approval`) the old memory gets `superseded_by` = new id and stops reaching prompts. Rejecting the new one leaves the old one untouched.
- `evidence` = the cited threads' urls with the PR number.

### 7.4 Memories in reviews

- `memories::applicable` excludes superseded memories. Prompts render memories grouped by kind, each with its index, content and the curated fields (bounded per memory).
- Finder / Verifier prompt rules (written fresh):
  - conventions: do not flag code that follows them; flag violations;
  - fix patterns: **actively look for** them — they are problems this repo has had before; report code with the same problem, with the recorded fix as the recommendation (user decision 2026-11-27);
  - exceptions & fix patterns: **do not drop** a finding because of them; when a finding matches one, cite its index in a new finding field `precedent`.
- **Post-processing in code** (the model only cites, code decides):
  - `precedent` → `exception`: severity forced to `info`; the comment body gets a "Precedent" block: `A similar case was declared <by design|won't fix> in PR #N: <reason>` + link.
  - `precedent` → `fix_pattern`: severity unchanged; the body gets `A similar case was fixed in PR #N: <resolution>` + link.
  - invalid or convention index → ignored.

## 8. Dashboard (`web/`)

- Memory list/detail shows `kind`, `context`, `reason`, `resolution` and evidence links; pending learned memories show which memory they update (diff of `content`). Edit form gains the new fields. Follows `web/AGENTS.md` and `DESIGN.md`; verified with `pnpm -C web check` and `curl` against `/api/v1/*` (no browser automation).
- API: memory endpoints accept/return the new fields; `kind` filter on list.

## 9. Limits & Costs

| Constant | Value | Where |
|---|---|---|
| Setting (`settings`, dashboard) | Default | Where |
|---|---|---|
| `max_thread_replies` | 5 Adus replies per thread; 0 = off | §6.3 |
| `max_validate_threads` | 30 threads per push; 0 = off | §6.4 |
| `max_learned_per_pr` | 10 memories per PR (≥ 1) | §7.3 |
| `curator_effort` | `medium` | §7.3 |
| Thread reply / validation model | provider's fast model, Low effort (no new setting) | §6 |
| Curator model | provider's review model (no new setting) | §7.3 |

The constants named `MAX_THREAD_REPLIES`, `MAX_VALIDATE_THREADS` and `MAX_LEARNED` in §6–§7 read these settings.

Each human reply in an Adus thread costs one small call; each push with changed finding files costs one small call per batch; each closed PR costs one medium call.

## 10. Testing

- `crates/git`: wiremock tests per provider for `post_review` ids, `reply_in_thread`, `set_thread_status` (GitHub GraphQL), `get_thread`; fixtures from real-shape payloads.
- `crates/server`: webhook parsing of reply events (GitHub `in_reply_to_id`, Azure non-root comment), marker-at-start rule, command precedence.
- `crates/review`: `ThreadReply` verdict handling (each verdict, empty reason, reply cap, not-an-Adus-thread), push validation (only `fixed` acts, errors don't fail review), curator validation (sources, kinds, `updates`, auto/approval status), precedent post-processing (Info downgrade, fix-pattern block). Reuse `FakeGit`, `test_support`, `#[sqlx::test(migrations = "../../migrations")]`. No redundant tests (AGENTS.md).

## 11. Builder's Calls

Left to the implementer, to be listed under "Assumptions" in their reports: exact label wording/emoji, prompt wording, batch sizes for push validation, how much code context per thread, the shape of `PostedComment` and `ThreadRef`, and GitHub GraphQL thread-id lookup details.

## 12. Final Task

Sync `docs/SPEC.md` (§ webhooks, jobs, git provider, engine, learning, schema, API) and mark this spec `Status: implemented`, adding it to SPEC.md §10.
