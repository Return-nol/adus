# Agentic review engine — risk & cost breakdown

Living note for `feat/advanced-agentic-review-flow` (updated 2026-11-22). Spec: `docs/superpowers/specs/2026-11-21-agentic-review-engine-design.md`. Plan: `docs/superpowers/plans/2026-11-21-agentic-review-engine.md` (Tasks 1–13 written; 14–15 decided, not yet written into the plan).

Token figures marked *estimate* come from the code limits (~4 chars per token). Figures marked *measured* come from live runs on `manot40/better-booru#11` (8 files, +102/−13, model `google/gemini-4-hydrogen`).

## 1. What a review costs

### Measured (better-booru#11)

| Run | Mode | LLM calls | Input tokens | Output tokens | Cache read | Wall time | Result |
|---|---|---|---|---|---|---|---|
| PR opened | Generalist | 7 (Finder only) | ~140K | ~6.6K | 108K | ~66 s | "No issues found." |
| `@adudu review deep` | Specialists | 35 | ~1.23M | ~87K | — | 7 min 12 s | "No issues found." + note "Bug reviewer did not finish in time" |

- Deep review ≈ **9× input, 13× output** of Generalist.
- Specialists at High effort: steps up to ~60 s each (max 66 s). Security finished in 8 steps, Performance in 12; Bug was cut by the Finder deadline at step 15/20 and its work was lost (fixed by Task 13).
- Old engine baseline (*estimate*): one single-shot call per file, no reasoning — ~10 calls / ~12–15K input for a PR this size.

### Per stage (*estimate*)

| Stage | Model / effort | Calls | Input tokens | Hard limits |
|---|---|---|---|---|
| Clone | — | 1 shallow fetch | — | 60 s, 500 MB/workspace |
| Finder, Generalist | primary, Medium (+4K reasoning reserve) | 1 agent per batch, 5–10 steps typical | 100–150K per batch (history re-sent each step) | 20 steps, compaction > 160K chars, 8,192 output/call, ~40K diff chars per batch, 2 batches in parallel |
| Finder, Specialists | primary, High (+8K) | 3–4 agents per batch | 3–4× Generalist | same |
| Verifier | fast model, Low (+2K) | ≤ 25 candidates × ~2–3 steps | up to ~375K | 6 steps, 25 candidates, 4 in parallel |
| Dedup | fast model, Low | 1 (only when ≥ 2 findings) | a few K | — |
| Small PR | primary, forced Generalist, no tools | 1 Finder call per batch + diff-only Verifier | ~3–5K + Verifier | 1–3 files, ≤ `small_pr_max_lines`, no risky path |

Cost drivers: re-sent agent history (prompt caching helps — Gemini read 108K from cache on the Generalist run), reasoning tokens (billed as output), Specialists multiplying the Finder, and retries (a retried review repeats clone + Finder + Verifier).

### Time budget

| | Generalist & small PR | Specialists (`review deep` / Specialists mode) |
|---|---|---|
| Review budget | 12 min (`REVIEW_TIMEOUT`) | **30 min** (`SPECIALISTS_REVIEW_TIMEOUT`, Task 13) |
| Stage deadlines | Finder 60 %, Verifier 90 %, Dedup 97 % of the budget | same fractions |
| Worker chain (Task 13) | `JOB_TIMEOUT` 31 min, `LOCK_TTL` 35 min, `reviews::STALE_AFTER` 35 min | |

Worst-case spend is bounded by time (and, after Task 14, by `max_review_input_tokens`), not by PR size.

### Infrastructure

Up to 500 MB temp disk per running review × workers; CPU for in-process grep (10 s budget per call); `git` ≥ 2.31 on the host (Docker image ships it; `adus doctor` warns). A Specialists review can now hold a worker for up to 30 min.

## 2. Risks

### Security

| Risk | Protection | Residual |
|---|---|---|
| Malicious PR code on the host | shallow fetch only, nothing executed; hooks, symlinks, fsmonitor, LFS filters, submodules, `file://`, credential helpers disabled | Low (Unix symlink/uid tests pass in Linux container) |
| PAT leak | PAT only in `GIT_CONFIG_*` env of `fetch`, scrubbed from errors | git helper child can keep the PAT env ~5 s after a timeout (backlog) |
| Path escape via tools | `RepoPath` containment, no `.git`, secret-file denylist | Low |
| **Bot quotes a committed credential in a comment** (prompt injection or plain mistake) | read-only tools, untrusted sections escaped, comments only on the same PR; Task 15 masks secret-like values in tool output, repo instructions and posted comments (the PR diff stays raw) | Low (pattern-based: a secret in an unknown format can still pass) |
| Proxy/CA env passed to git | fixed allowlist, no `GIT_*` trace vars | Low |
| Webhooks blocked upstream (seen: Cloudflare 403) | — | user adds a WAF skip rule for `/webhooks/*`; Adus never sees these requests |

### Correctness & behaviour

- Verifier is fail-closed: fewer false positives, some true findings dropped (counted in a note).
- Semantic Dedup by the fast model may merge distinct issues; on error it keeps everything.
- Partial reviews: Specialists posts survivors with a note; Generalist retries when every Finder failure is retryable; deadline cuts never retry. Task 13 makes slow agents submit before the deadline instead of losing their work.
- Small-PR heuristic: substring risky words fail safe (`author.rs` → full review). Open: a tiny follow-up push to a large PR counts as small.
- Multi-line anchors not yet verified live (GitHub range acceptance + Apply, Azure end offset, Azure suggestion fences). A GitHub 422 fails the whole review.
- Parked regression: Verifier snippet can miss the end line on spans > ~160 lines.

### Provider compatibility

Unverified live: final-step history (tool calls kept while only submit is offered), Gemini schema subset, Anthropic thinking-block replay, vLLM top-level `message` markers (backlog). OpenAI-compatible tool calls without ids are handled. Fast model without tools → diff-only Verifier.

### Operational

- Rate-limit amplification: per worker up to 2 batches × 4 agents + 4 Verifier calls → more 429s, cooldowns and full-review retries.
- 30-min Specialists reviews can occupy both workers; a second event on the same PR waits for the lock (35 min TTL); stale rows are failed after 35 min (was 15).
- Commands only from registered users: GitHub matches `users.github_login` (seen: admin without a login → "unregistered author", logged at info only).
- Flaky server test `discover_models_with_inline_key…` (backlog).

### Maintenance

~+16K lines over ~32 commits, ~600 tests; 12-module review pipeline; logs are content-free by design (use `RUST_LOG=info,adus_review=debug` for per-step tool names and agent outcomes). Backlog: `tools.rs`/`workspace.rs` ~800 lines each, four diff walkers, regex size limit, runtime orphan sweep, test gaps.

## 3. Mitigations — status

### Tier 1 (configuration / setup)

| Mitigation | Owner | Status |
|---|---|---|
| Generalist as global default; Specialists via `review deep` or per-repo | user | to do |
| Cheap `fast_model` (Verifier + Dedup) | user | to do |
| More keys per provider | user | to do |
| Cloudflare WAF skip rule for `/webhooks/*` | user | to do (webhook resynced, deep review worked) |
| Dedicated bot account + fine-grained PAT | user | to do |
| Worker count vs 30-min deep reviews | user | to do |
| Gradual rollout + click-test multi-line Apply | user | to do |
| **Cap PR size defaults:** `max_files_per_review` 50 → 30, `max_patch_lines_per_file` 2000 → 1500 | Claude, Task 14 | done |
| **Small-PR threshold default** 50 → 100 lines | Claude, Task 14 | done |

Defaults change in the new migration 0009 (0003 is already on `origin/feat/adus-v1` and 0008 stays as it is, so no checksum repair is needed): `SET DEFAULT` plus an `UPDATE` of the settings row where it still holds the old default, so a value an admin changed is kept. The dev DB gets it on the next server start.

### Tier 2 (code)

| # | Mitigation | Task | Status |
|---|---|---|---|
| — | Deadline-aware wrap-up (time or last 2 steps) + one step-budget note; 30-min Specialists budget; worker chain 31/35/35 | 13 | implementing |
| 1 | Per-review input-token ceiling: setting `max_review_input_tokens` (default 2,000,000, 0 = off); when crossed, Finder agents wrap up and remaining Verifier candidates are dropped with a note | 14 | done |
| 2 | Secret redaction: mask API keys, tokens, private keys, `password=`-style values in tool outputs and repo instructions before the LLM, and in posted comment bodies; PR diffs are not masked so a hardcoded secret can still be flagged | 15 | done |
| 3 | Record `cache_read_tokens` / `cache_write_tokens` in `llm_usage` and show them in usage totals | 14 | done |
| 4 | Skip a review whose PR head changed while queued (check whether already done) | — | not chosen yet |
| 5 | Log every webhook Adus rejects or ignores (event, reason, never the body) | — | not chosen yet |
| 6 | Cheaper Specialists (Performance at Medium, step cap ~14) | — | not chosen yet |
| 7 | Smaller per-step context (compaction 160K → 100K chars, smaller `read_file` window) | — | not chosen yet |

### Tier 3 (backlog)

Kill git's helper child on clone timeout; runtime orphan-workspace sweep; Verifier long-span snippet; split `tools.rs` / `workspace.rs`.

## 4. Decisions on record (2026-11-21 → 22)

- Tools keep honouring `.gitignore`.
- Generalist retries when any Finder failure is retryable; Specialists keeps partial posting.
- Small PRs: Finder diff-only (legacy per-file prompt deleted), always Generalist; commands bypass.
- Docker only for final gates (Linux compile/test check passed at `0c784d9`).
- Specialists budget 30 min; deadline-aware wrap-up (Kodus community code consulted for behaviour only: `force-finalize.policy.ts`, `budget.policy.ts`, `llm-call.ts`; Kodus has no time-based wrap-up and drops an agent's findings on timeout).
- Token ceiling is a setting (default 2M), not a constant.

## 5. Resume pointer

- Branch `feat/advanced-agentic-review-flow`; latest commits `37ede53` (plan Task 13) on top of `5a6bc5f` (Task 11). Nothing pushed; `.cargo/config.toml` stays uncommitted.
- SDD ledger: `.superpowers/sdd/2026-11-21-agentic-review-engine/progress.md` (tail lists Task 13 dispatch and the Task 14/15 decisions).
- Next: Task 13 report → review → (fix loop) → write Tasks 14 and 15 into the plan → implement + review each → dev DB repair (new 0008 checksum, settings row 30/1500/100) → Linux gate only if Unix code changed.
