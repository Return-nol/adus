# Handoff — Thread Conversations & Curated Learning (after Task 7 of 13)

Date: 2026-11-27 · Branch: `feat/thread-conversations` (from `origin/main` b6b9a4d, **no upstream set, never pushed**)

Read first, in this order:
1. `AGENTS.md` (repo rules, clean-room rule, `-j` / Windows gotchas)
2. Spec: `docs/superpowers/specs/2026-11-27-thread-conversations-and-curated-learning-design.md` (binding)
3. Plan: `docs/superpowers/plans/2026-11-27-thread-conversations-and-curated-learning.md` (Tasks 8–13 remain)
4. This file (rulings that override the plan text)

## Done (Tasks 1–7, each implemented + reviewed + fixes re-reviewed)

| Task | Commits | What exists now |
|---|---|---|
| 1 Bot label | 78ea68e | `adus_core::types::is_adus_comment` (marker must be first); `prompt::adus_body(label, text)`, `prompt::REPLY_LABEL`; inline `🤖 **Adudu** · **⚠️ Warning:** <title>`, summary `🤖 **Adudu** reviewed this PR` |
| 2 Migration + DB | 6b84222, 2fe3b77 | `migrations/0010_thread_conversations.sql`; `adus_db::review_threads` (insert_many, find, open_for_pr, for_pr, set_outcome, record_reply, set_validated); memories `kind/context/reason/resolution/evidence/supersedes/superseded_by`; 4 settings columns |
| 3 GitHub thread ops | a9fb167, bbd20e6 | `PostedComment`, `ThreadRef`, `ThreadStatus`; `PrThread.id/resolved`, `PrThreadComment.id`; `reply_in_thread`, `set_thread_status` (GraphQL), `get_thread` |
| 4 Azure thread ops | 6a43956, a2c5409 | same trait methods on Azure |
| 5 Record threads | 06d629c, 439fbcc | engine writes `review_threads` rows after posting (outside the review deadlines) |
| 6 ThreadReply job | b5dae66 | `Job::ThreadReply { repository_id, pr_number, thread_id, comment_id }` from webhooks, worker routes under the per-PR lock |
| 7 thread_reply | 3c328fc, 5218833 | `crates/review/src/threads.rs`: answers replies in Adus threads, verdicts fixed/not_fixed/wont_fix/by_design/needs_reason/discussion, sets provider status, syncs human-resolved threads, HELP fallback for `@adudu <unknown>` in non-Adus threads |

Not yet verified: one full `cargo test --workspace` run over all of Tasks 1–7 together (each task ran only its own crates). Run it before starting Task 8. Known flaky (pre-existing, timing under load): `finder_deadline_*`, `slow_specialist_submits_before_the_finder_deadline`, `clone_is_bounded_by_the_finder_deadline`.

## Remaining: Tasks 8–13 (plan text + rulings below)

8. Push validation, 9. Memory kinds + precedents in reviews, 10. Curator (`learn_pr` rewrite), 11. API, 12. Dashboard, 13. Sync `docs/SPEC.md` + mark spec implemented. Then a whole-branch review.

## Rulings that override or extend the plan (binding for Tasks 8–13)

- **Ids.** `review_threads` is UNIQUE on `(repository_id, pr_number, provider_thread_id)`. GitHub `provider_thread_id` = root review comment id, forever (GraphQL node id is looked up per call, never stored, there is no `set_provider_thread_id`). Azure = thread id.
- **post_review** returns exactly one `PostedComment` per inline comment, in order. `comment_id` is empty when unknown; rows are only written for non-empty ids.
- **Supersede (Task 10/11).** `activate_superseding` does NOT exist. `memories::insert` with `status = active` and `supersedes = Some(id)` retires the target atomically; `approve` does the same for pending. Both skip a target that is already superseded. Task 10's `updates` validation must reject a target with `superseded_by` set.
- **Task 8 (push validation).** GitHub `list_pr_threads` always reports `resolved = false`. To sync human-resolved threads, call `get_thread` per candidate row (bounded by `max_validate_threads`). Reuse the human-status mapping already in `threads.rs` (`human_outcome`) rather than writing a second one.
- **Task 9.** `apply_precedent` picks "by design" when the memory's `context` starts with `By design:`, else "won't fix". Task 10's curator must write that prefix for exceptions.
- **Spec drift to fix in Task 13.** `ThreadReply` carries no `author` (spec §6.3 still lists it). Spec §4.1 inline label is already corrected in the working tree (uncommitted, see below).
- **Build speed (user-approved).** Use `cargo … -j 6` (fall back to `-j 4` on `os error 1455`). Each task runs only its touched crates' tests (`cargo test -p <crate> -j 6 --test <file>`), plus `cargo check --workspace --all-targets -j 6`. Dev DB: `docker compose -f docker-compose.dev.yml -p adus-dev up -d` (never touch the default compose project "38").
- **Commits.** Conventional Commits ending with `Co-Authored-By: <model> <noreply@anthropic.com>`; never push without the user asking.

## Deferred minors (for the whole-branch review to triage)

- Task 1: stale code comment `prompt.rs` ("inside the bold label").
- Task 2: `MemoryPatch` uses COALESCE, so `context/reason/resolution` can't be cleared to NULL (revisit in Task 11/12); `crates/server/tests/doctor.rs` hard-codes the new settings defaults.
- Task 3: `fullDatabaseId` may be missing on older GitHub Enterprise Server; note it in SPEC.
- Task 4: Azure PATCH status uses camelCase strings; if Azure rejects them, use numeric `CommentThreadStatus` (active=1, fixed=2, wontFix=3, byDesign=5). Verify against a live PR.
- Task 6: Azure thread-id parser (`webhooks.rs`, `_links.threads.href`) is brittle on a trailing slash or query string; reply detection could also accept `id > 1`.
- Task 7: no test for the 20-comment prompt cap or the shifted index; a row fixed via human-status sync has no `fix_sha`; `not_fixed` on an open row stores nothing (spec goal 6 vs §6.3).

## Uncommitted in the working tree

- `docs/superpowers/specs/2026-11-27-thread-conversations-and-curated-learning-design.md`: §4.1 inline label corrected to `**⚠️ Warning:**`. Commit it with Task 13 or on its own.

## Housekeeping after the feature is done

- `target/` is ~16 GB (`debug/incremental` ~6.5 GB). After the final review, and never during a build, delete `target/debug/incremental` or run `cargo clean`.
- The SDD ledger with per-task reports and review diffs is in the git-ignored `.superpowers/sdd/2026-11-27-thread-conversations-and-curated-learning/` (`progress.md`, `task-N-brief.md`, `task-N-report.md`).

---

## Validation of Tasks 8–13 (as of e986731, 2026-11-27) — open items

Gates: fmt ✅ · clippy -D warnings ✅ · cargo test --workspace ✅ except 4 known-flaky deadline tests (pass on rerun / solo) · pnpm -C web check ✅ 0/0 · web fmt ✅

### Important (fix before merge)
1. engine.rs:286-302 — push validation has no own deadline and eats the review budget (start/cut computed before it). Wrap validate_open_threads in tokio::time::timeout (~2 min), treat elapse as 0, log.
2. threads.rs:575 — push validation ignores max_thread_replies (0 = never reply; also cap per row). Still set status/row, skip reply_in_thread when capped. Add test.
3. learn.rs:117 is_adus_fixed — human-resolved rows (outcome Fixed, fix_sha None) count as "confirmed fixed by Adus" → auto-active fix_pattern without evidence. Require fix_sha.is_some().
4. learn.rs:511 — exception `By design:`/`Won't fix:` prefix only prompted, not enforced; derive from cited Adus row outcome or default `Won't fix: `. Add test assertion.
5. docs/SPEC.md:84/:265 — missing note: existing pr_learning memories no longer suppress, now Info + Precedent; fallback (no evidence → no PR#, source_url link; empty reason/resolution → content).

### Minor (triage before merge)
- threads.rs:502 validation has no input-size budget; non-retryable failure re-sends every push (set_validated after non-retryable error or cap batch chars).
- threads.rs:412 get_thread NotFound row re-fetched every push (set_validated).
- prompt.rs:253 memory content cut to 500 chars (regression for 2,000-char memories) — cap only curated fields.
- learn.rs:529/:61 one bad `updates`/kind/thread drops memory or whole answer — ignore bad updates, lenient item parse.
- learn.rs:398 within_curator_budget quadratic + regex on async thread; cap candidates.
- tests: max_validate_threads=0/cap, Command-trigger validation, non-default curator_effort.
- engine.rs:287 models() lookup before draft skip (DB error fails a draft run).
- prompt.rs:326 `By design:` match case-sensitive, untrimmed.
- threads.rs validation_prompt rename: use matched file_diff.path for code_context.
- SPEC.md:285 curator doesn't call approve; SPEC.md:307 "diff" overclaims + kind filter unmentioned.
- web rules page: superseded memories look live (add badge); kind filter reloads Rules tab; empty-state text; Pending tab extra fetch failure kills section.
- memory-dialog.svelte:91 edit-and-approve = 2 requests.
- settings page: section instead of separate Card; resetIgnore sends 4 dead fields.
- api/memories.rs:147 error text "between 1 and 2000" → "at most 2000".
- Earlier deferred (handoff list): Azure thread-id parser hardening; Azure PATCH camelCase vs numeric; 20-comment cap test; fixed-via-sync no fix_sha; GHES fullDatabaseId.

After fixing: re-run fmt, clippy -D warnings, `cargo test --workspace` (4 timing tests flaky under load — rerun solo), `pnpm -C web check`, then a whole-branch review before merge.
