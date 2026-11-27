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
