# PR Discussion Awareness & Learning from Closed PRs — Design Specification

Date: 2026-11-23 · Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §1, §3.1, §4.3, §5, §7, §8.4, §8.5, §9)

> **Binding scope & policy:** Extends [docs/SPEC.md](../../SPEC.md) and the agentic review engine spec ([2026-11-21-agentic-review-engine-design.md](2026-11-21-agentic-review-engine-design.md)). The clean-room and no-assumption rule in `AGENTS.md` applies: Kodus community code may be read as a behaviour reference only (cite it), Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened. All prompts, schemas and code are written fresh for Adus.

---

## 1. Intent & Why

Two problems the user hits today:

1. **Repeated comments.** When a PR gets new pushes, Adus reviews again and can post a finding that is already on the PR — raised earlier by Adus itself, by a human reviewer, or already discussed and rebutted in a thread. That is noise and erodes trust.
2. **Forgotten "by design" decisions.** A reviewer (Adus or a human) flags code A as wrong; the developer answers that A is intentional — required by the PRD or by the team's coding standard. On the next PR with the same pattern, Adus flags it again. The user wants Adus to learn these exceptions so that, next time, the same pattern is not commented on.

**Success:** on a PR with new pushes, Adus does not re-post what is already being discussed; after a PR closes, the "by design" explanations from its discussion become memories (after admin approval, unless the repo is set to auto), and later reviews stop flagging those patterns.

## 2. Goals, Non-Goals, Anti-Goals

### Goals

1. **Part 1 — skip findings already raised on the PR.** Before posting, every review compares its verified findings with the PR's existing comments (from anyone: Adus, human reviewers, other users) and drops the findings an existing comment already covers.
2. **Part 2 — learn exceptions when a PR closes.** When a PR is closed (merged or abandoned), Adus reads its discussion threads, extracts "this is intentional because …" exceptions (from threads on Adus findings and on human reviewer comments), and stores them as **repo-scoped memories**.
3. **Approval per repository.** Each repository has a learning mode: `approval` (default — learned memories wait for an admin to approve them), `auto` (learned memories are active at once when a registered Adus user defended them, see §5.3), or `off`.
4. Learned memories are visible, editable, approvable/rejectable and disableable in the existing Memory UI. **Only admins** approve, reject, edit or delete a `pending` or `rejected` memory; everything else about memories keeps today's rules (role `user` still manages active memories; manual memories and `@adudu remember:` stay active at once — user decision 2026-11-23).
5. **Rejections are remembered.** A rejected learned memory keeps status `rejected`: never injected into review prompts, hidden from the default list, and passed to the learning extraction as "do not propose again".

### Non-Goals (this wave)

- Replying in PR threads (no "noted, I'll remember this" comment). Learning is silent.
- GitHub "resolved conversation" status (GraphQL only); GitHub threads are treated the same whether resolved or not.
- Learning from PR descriptions, commit messages or code changes — only discussion threads.
- Global-scope learned memories (always repo scope; an admin can still create global memories by hand).
- A cap or relevance selection for memories injected into prompts (see §8, risk).
- The deferred "Point 3" `review_findings` table: Part 1 works from the live PR comments instead, which also covers human comments.

### Anti-Goals (failure even if it "works")

- A developer silencing real bugs: an unreviewed "by design" reply must never suppress findings in `approval` mode.
- A PR comment steering the reviewer by prompt injection (e.g. "drop all findings") and wiping a review.
- Leaking secrets that appear in comments to the LLM or into memories.
- Making every review noticeably more expensive: Part 1 adds at most one small fast-model call per review; Part 2 adds at most one per closed PR, and none when there is nothing to learn.

## 3. Approaches Considered

### Part 1 (skip already-raised findings)

| | Approach | Trade-off |
|---|---|---|
| 1A | Existing comments in the Finder and Verifier prompts | Most thorough, but every LLM call grows; the Finder may also under-report new issues that look similar |
| **1B (chosen)** | **Extend the existing Semantic Dedup stage** with the PR's existing comments | One small fast-model call per review; reuses the stage whose job is removing duplicates |
| 1C | Deterministic match (same file, nearby line, similar title) | Free, but misses moved code and reworded comments |

### Part 2 (learn exceptions)

| | Approach | Trade-off |
|---|---|---|
| **2A (chosen)** | **Store exceptions as memories** with a new status (`pending`/`active`) and a new source `pr_learning` | Reuses table, API, UI and the prompt rule "do not flag code that follows [memories]"; memory list grows (prompt cost) |
| 2B | Separate exceptions table used only by the Verifier | Leaner Finder prompt, but a new table and page, and the Finder still spends tokens finding what gets dropped |
| 2C | Store as Rules with an "ignore" severity | Can use `path_glob`, but inverts what a Rule means |

The user picked 1B + 2A, approval by default with a per-repo auto option, and counting comments from everyone.

## 4. Part 1 — Discussion-Aware Dedup

### 4.1 Reading PR threads (`crates/git`)

New `GitProvider` method, implemented for GitHub and Azure, with `FakeGit` support:

```rust
async fn list_pr_threads(&self, repo: &RepoRef, pr_number: i64) -> Result<Vec<PrThread>, GitError>;

pub struct PrThread {
    /// File the thread is anchored to (normalized, no leading `/`); `None` = general PR comment.
    pub path: Option<String>,
    /// Line on the new side when the provider still knows it; `None` when outdated or general.
    pub line: Option<u32>,
    /// Azure thread status (`active`, `fixed`, `wontFix`, `closed`, `byDesign`, `pending`); GitHub: `None`.
    pub status: Option<String>,
    /// Link to the thread (first comment), http(s) only.
    pub url: Option<String>,
    pub comments: Vec<PrThreadComment>, // oldest first
}

pub struct PrThreadComment {
    pub author: String,     // GitHub login / Azure uniqueName
    pub by_adus: bool,      // body contains `<!-- adudu -->`
    pub body: String,
}
```

- **GitHub:** review comments (`GET /pulls/{n}/comments`, grouped into threads by `in_reply_to_id`; `path`, `line`), issue comments (`GET /issues/{n}/comments`, one thread each, no path) and non-empty review bodies (`GET /pulls/{n}/reviews`). Comments by `user.type == "Bot"` are skipped (CI, linters); Adus posts as the PAT user, so its comments stay and are recognized by the marker.
- **Azure:** `GET …/pullRequests/{id}/threads`. Skip `commentType == "system"` and deleted comments, and threads left empty. `path` from `threadContext.filePath` (normalized), `line` from `rightFileStart.line`, `status` from the thread.
- Same `send`, pagination cap and error mapping as the other calls. Comment bodies are never logged.

### 4.2 Engine changes (`crates/review`)

- The engine fetches the threads once per review run, for every trigger (opened, updated, command) and every path (full, small PR, diff-only fallback). A fetch error is logged (`warn`, no body) and the review continues without existing comments — it never fails the review.
- **Dedup runs** when there are verified findings and either ≥ 2 findings (as today) or ≥ 1 existing comment.
- **Input to Dedup:** the verified findings plus an `<existing_comments>` section: threads whose `path` is a file of some finding, plus general threads. Each comment is passed through `redact_secrets`, its body is cut to ~1,000 chars, and the section holds at most ~60 threads (same-file threads first, newest first). It is wrapped as untrusted data like every other section.
- **Output:** `{"keep_indices": [...], "already_raised": [{"finding": i, "comment": j}, ...]}`. A finding is dropped as already raised **only** when it appears in `already_raised` with a valid comment index. A drop without a cited comment is ignored (keep the finding). Every other part of today's dedup behaviour stays (invalid answer or error → keep everything).
- **Summary:** when ≥ 1 finding was dropped this way, add a line: `N finding(s) skipped: already raised in this PR's discussion.`
- Adus's own earlier findings, including ones the developer rebutted in a thread, are existing comments, so a re-push never re-posts them.

## 5. Part 2 — Learning on PR Close

### 5.1 Trigger

- **GitHub:** `pull_request` with `action == "closed"` (merged or not) → `Job::LearnPr { repository_id, pr_number }`.
- **Azure:** `git.pullrequest.updated` whose `resource.status` is `completed` or `abandoned` → `Job::LearnPr` (today these are dropped). No new service-hook subscription is needed.
- The worker runs it under the same per-PR lock as reviews. It is a short job (one LLM call) under the existing `JOB_TIMEOUT`.
- **Run once per PR head:** a new table records learning runs, unique on `(repository_id, pr_number, head_sha)`. A repeat event for the same head is skipped; a reopened PR with new commits that closes again is learned again (only new exceptions are added, see dedup below).

### 5.2 What qualifies

- Repo is active and its `learning_mode` is not `off`; a provider with a model is configured (else skip quietly, no comment, no failure).
- Fetch threads (§4.1). A thread qualifies when it has **at least two comments from at least two different authors** (a finding and a reply); comments with `by_adus` count as one author of their own ("adus"), whatever account posted them. No qualifying thread → no LLM call, run recorded as `nothing_to_learn`.
- Every commenter counts for extraction (anyone can explain a design decision); who defended an exception only matters for activation in `auto` mode (§5.3).

### 5.3 Extraction

- One single-turn call to the provider's `effective_fast_model()` with `ReasoningEffort::Low`, usage logged like other calls.
- Input: qualifying threads (redacted, bounded like §4.2, with path/line and Azure status — `byDesign`/`wontFix` are strong hints; each comment carries its index within the thread), plus the repo's current memories (active and pending) so it does not repeat what is already known, plus the repo's `rejected` learned memories marked "do not propose again".
- Instruction (written fresh): extract only exceptions where the discussion establishes that the flagged code is **intentional**, with the reason (PRD, team standard, external constraint). Write each as a general, reusable statement about the codebase that names the pattern and the reason, not the PR. Skip threads where the flag was accepted, fixed, unresolved or only argued without a reason.
- Output: `{"exceptions": [{"content": "...", "thread": <index>, "defended_by": [<comment index>, ...]}]}` — `defended_by` names the comments in that thread that state the design reason. An exception with an invalid thread index or no valid `defended_by` is discarded. At most 5 per PR, each ≤ 2,000 chars (the `remember` limit). Invalid answer or error → nothing stored, run recorded as `failed` (no retry storm: `Retryable` only for `AllKeysFailed`/`NoKeys`, like other jobs).
- Each exception becomes a memory: `scope = repo`, `source = pr_learning`, `source_url` = the thread's url, `created_by` = the registered Adus user who defended it (else `NULL`), and a status:
  - mode `approval` → `pending`, whoever defended it;
  - mode `auto` → `active` **only** when at least one `defended_by` comment's author is a registered, active Adus user, matched with the same rule as `@adudu` commands (GitHub `users.github_login`, Azure email = `uniqueName`, both case-insensitive); otherwise `pending`. The check is done in code from the comment authors, never from the model's word.
  - *Implementation refinement (final review):* `auto` additionally requires every non-Adus comment author in that thread to resolve to a registered, active Adus user; otherwise `pending`. This stops an unregistered commenter's "by design" reason from going live because the model cited an unrelated maintainer reply. The threads section is cut to 40,000 chars and the known/rejected memory lists to 20,000 chars each (newest first); the answer cap is 8,192 tokens (see `docs/SPEC.md` §8.5).

## 6. Data Model (migration `0009`)

```sql
ALTER TYPE memory_source ADD VALUE 'pr_learning';
CREATE TYPE memory_status AS ENUM ('pending', 'active', 'rejected');
ALTER TABLE memories ADD COLUMN status memory_status NOT NULL DEFAULT 'active';

CREATE TYPE learning_mode AS ENUM ('off', 'approval', 'auto');
ALTER TABLE repositories ADD COLUMN learning_mode learning_mode NOT NULL DEFAULT 'approval';

CREATE TABLE pr_learning_runs (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id uuid NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pr_number     bigint NOT NULL,
    head_sha      text NOT NULL,
    outcome       text NOT NULL,          -- learned | nothing_to_learn | skipped | failed
    learned_count integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repository_id, pr_number, head_sha)
);
```

- `memories::applicable` adds `AND status = 'active'`, so pending and rejected memories never reach a prompt.
- Existing memories become `active` (column default), so nothing changes for them.
- (`ALTER TYPE … ADD VALUE` cannot run inside a transaction block on older Postgres; check sqlx migration behaviour on PG 18 and split the migration if needed — builder's call.)

## 7. API & Dashboard

- **Memories API:** responses include `status` and `source`. New admin-only actions: approve (`pending` → `active`, content may be edited in the same call) and reject (`pending` → `rejected`). The list hides `rejected` by default and supports a `status` filter. The existing PATCH and DELETE refuse a `pending` or `rejected` memory for non-admins (`403 forbidden`); for `active` memories they keep today's rules.
- **Repositories API:** `learning_mode` readable by everyone, settable by admins (`off | approval | auto`).
- **Memory UI (Review Rules page):** a "Pending" group/tab with a count badge (across all repos), each item showing the content, the repo, a link to the source thread, and Approve / Edit / Reject (admin only; role `user` read-only on pending). A "Rejected" view reachable from there (admin can delete for good, which lets it be proposed again). Learned memories show a "Learned from PR" label. Skeletons updated with the layout (web/AGENTS.md).
- **Repositories page:** a learning-mode select per repo (admin only), next to the review-mode control.

## 8. Risks

- **Prompt growth:** with `auto` (or many approvals) the injected memory list keeps growing; every Finder and Verifier call pays for it. Mitigation now: approval default, the 5-per-PR cap and the dedup against existing memories. A cap or relevance selection is backlog.
- **Over-merging in Part 1:** the fast model may treat a new, different problem as "already raised" and drop it. Mitigated by the cited-comment requirement; the summary line makes it visible.
- **Reintroduced issues:** an issue fixed earlier and reintroduced on the same PR may be dropped as already raised. Accepted.
- **Injection via comments:** comments are untrusted data, redacted and wrapped; drops need a citation; learned memories in `approval` mode are reviewed by a human.

## 9. Calls Made by the Designer (not discussed with the user)

- Learning runs on both merged and abandoned PRs; one fast-model call per closed PR, none without a qualifying thread.
- Learned memories are always repo-scoped; `learning_mode` is per repo only (no global setting), default `approval`, with an extra `off` value.
- Part 1 also applies to `@adudu review` commands; the summary line reports how many findings were skipped.
- Thread fetch failures never fail a review; extraction failures never post anything.
- Learning is silent (no reply in the thread).

## 10. Left to the Builder

- Exact prompts (written fresh), section bounds within the stated limits, and the thread ordering heuristic.
- Whether `list_pr_threads` paginates the three GitHub endpoints concurrently.
- API route shapes for approve/reject (following existing `ApiJson`/`ApiPath` patterns and error codes).
- Tests per the AGENTS.md testing rule (no redundant tests; reuse `FakeGit`, `ScriptedChat`, fixtures).
- Final plan task: **sync `docs/SPEC.md` and mark this spec implemented** (AGENTS.md workflow rule).
