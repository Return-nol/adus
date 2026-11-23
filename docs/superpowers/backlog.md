# Backlog — deferred minor findings

Collected from the per-task and final reviews of the v1 plan, the follow-up plan (ignored files + Azure project names) and the agentic review engine plan. None blocked merge. Fix opportunistically.

## v1 plan
- Task 4: minor (deferred): body-too-large maps to 400 not 413
- Task 5: minor (deferred): session delete runs after commit (500 after applied change on failure); missing 403/401 tests for POST/PATCH users and /me*
- Task 5: minor (deferred): collapsed whitespace in UPDATE literal crates/db/src/users.rs:141
- Task 6: minor (deferred): click-through UI never browser-verified — user to test manually; loadSession error invisible on /login
- Task 7: minor (deferred): PullRequest/FileDiff/InlineComment/FileStatus lack serde derives — add when a task serializes them
- Task 9: minor (deferred): Azure post_review stops at first failed thread (dup on retry — see Task 21 note); 302-to-signin surfaces as Decode; no content size cap; Azure webhook basic-auth consumerInputs unused (URL token instead)
- Task 10: minor (deferred): PATCH token+name not atomic; set_webhook_ids error in sync_webhooks aborts loop (should become webhook_error); Unauthorized from ensure_webhooks after rotation marks connection invalid (ambiguous / stale resync race); add-repo half-built window; connection delete hard-deletes soft-deleted repos (revisit when reviews FK lands, Task 21); add-repo vs connection delete race → 500; webhook_token stored plaintext (plan-mandated TEXT; consider Cipher); N+1 repo count
- Task 11: minor (deferred): PAT stays in connection-dialog state after failed submit/cancel until reopen (clear on close); first-load failure shows empty-state text; multi-line toast collapses; no busy guard on resync/toggles; failed picks cleared; alert action label always Delete
- Task 12: minor (deferred): provider error.message forwarded verbatim (compatible server could echo key); Gemini model unescaped in URL path
- Task 13: minor (deferred): candidate_keys+get_provider not in one tx
- Task 14: minor (deferred): NULL repository (key-test pings) labelled '(deleted)' in breakdown — consider '(no repository)'; no test for upstream 4xx echoing key; no TZ-shifted session test; provider_in_use+delete not atomic; N+1 keys per provider
- Task 15: minor (deferred): key value kept in state after failed save until close; busy.delete outside finally; fail() duplicated in 4 pages (move to api.ts); chart colors near-gray; Select value='' None unverified at runtime; sidebar /token-usage→/usage fixed
- Task 16: minor (deferred): no test asserts updated_at advances; ACTIVE_REPO const duplicated in rules.rs/memories.rs; update/delete by id don't check active repo; rule_matches_path compiles glob per call (ponytail)
- Task 17: minor (deferred): no unit test for safeHttpUrl; scope dropdown includes disabled repos
- Task 18: minor (deferred): XAUTOCLAIM from 0-0 COUNT 1 scans ~10 PEL entries (fine at 2 workers); ~~jobs >15 min get reclaimed while running~~ (resolved in Task 13: `RECLAIM_IDLE` 35 min > `JOB_TIMEOUT` 31 min, compile-time assert)
- Task 18: minor (deferred): empty XPENDING after claim delivers anyway (at-least-once dup, very unlikely)
- Task 19: minor (deferred): GitHub delivery id unsigned → replay with new id bypasses dedup (accepted; optional sha256(body) key); ~~test cleanup leaks streams on failed assertion~~ (resolved in post-implementation tidy-up: `TestQueue` `Drop` guard in `crates/review/tests/command.rs`)
- Task 20: minor (deferred): first non-empty array could be an example array; title blank lines → double spaces
- Task 21: minor (deferred): 'no model configured' comment repeats on every push (plan-mandated)
- Task 21: minor (deferred): @adudu review on delete-only PR gives no visible feedback (Skipped row only)
- Task 22: minor (deferred): GitHub email fallback in authorize is dead code/latent risk (plan-mandated; doc it); blockquote escapable with bare \r and re-pings @mentions; 'please review' → Unknown; mentions inside code/quotes trigger; missing-repo path untested
- Task 23: minor (deferred): shutdown race strands a delivery until reclaim (now 35 min, +1 attempt) — alternative is_cancelled check at loop top; ~~reclaim duration not exposed for const assert~~ (resolved in Task 13: `adus_queue::RECLAIM_IDLE`)
- Task 23: minor (deferred): Azure timeout mid-post → Retryable dup threads (pre-existing; split prepare/post phases, Fatal after posting starts on Azure); panic inside review_pr leaves row running; server failure exits code 0; abort() not awaited before unlock; rows left running after 30 s shutdown grace
- Task 24: minor (deferred): repo filter omits soft-deleted repos; filter change flashes empty table
- Task 25: minor (deferred): localhost warning misses [::1]; check_git list-error branch untested
- Task 26: minor (deferred): env_file passes POSTGRES_PASSWORD/COMPOSE_PROFILES into adus container; interactive prompt + cold build untested
- Task 26: minor (deferred): README !override snippet needs Compose 2.24+; empty compose --short passes check silently
- Task 27: minor (deferred): release.yml applies latest to pre-release tags (drop type=raw latest, rely on flavor latest=auto); release doesn't gate on CI; arm64 via qemu slow (web stage --platform=$BUILDPLATFORM, timeout-minutes)

## Follow-up plan
- Task 1: minor (deferred): reviews API repository.name now carries display name (misleading field name)
- Task 2: minor (deferred): ~~unused name field in repositories page Repo type~~ (resolved in post-implementation tidy-up); no 'N selected' hint for hidden selections
- Task 3: minor (deferred): GlobSet build failure mapped to InvalidGlob(message); settings replace read-then-write not transactional
- Task 4: minor (deferred): TooLong/TooMany pattern + >500 keywords + [] keywords branches untested; settings PUT read-then-write race
- Task 6: minor (deferred): confirm action no busy guard; keywords not validated client-side
- Task 7: minor (deferred): fail_stale inside select arm stalls promote during DB outage
- fail_stale SQL string is one long line (cosmetic)

## Agentic review engine (plan 2026-11-21)
Task numbers below are this plan's tasks. Sources: the per-task reviews, final review #1 (Tasks 1–10, 12) and final review #2 (Tasks 11, 13–15); items those reviews or the fix waves resolved are left out.
- Task 1: minor (deferred): `@adudu review generalist` not run through `handle_comment` in a test
- Task 2: minor (deferred): merge loop duplicated in `anthropic.rs`/`gemini.rs`, tool-declaration mapping ×3; `error_text` ignores a top-level `message`/`detail` string (vLLM-style errors), so only overflow detection would gain — add with a vLLM fixture
- Task 2: minor (deferred): OpenAI/Gemini still send an empty system message when `system` is empty (only `test_key`)
- Task 3: minor (deferred): `git-remote-https` can outlive git on clone timeout (kill the process group/job object); failed workspace removals wait for the restart sweep (add the sweep to `run_scheduler`); no tests for ADS `:stream`, trailing-dot secret names, short names; split `workspace.rs` (path half to its own file)
- Task 3: minor (deferred): document that the clone-root guarantee assumes a sticky / not other-writable parent (shared non-sticky `TMPDIR` is not covered) and that uid-mapping filesystems (bind/FUSE) fail closed into diff-only
- Task 3: minor (deferred, live check): Azure git clone URL with the project GUID not verified against a real organisation (a failure only degrades to diff-only)
- Task 4: minor (deferred): split `tools.rs` (defs / search; `read_text` beside `RepoWorkspace`); excerpt marker " [line truncated]" also used for non-line cuts; `grep_file` builds full vectors per file (bounded); one uncancellable `is_match` on a 64 KiB line can still run 6–16 s past the budget (tune the window with numbers); `find_files` prints "No files match" plus the time note when cut before any match; Io message not asserted in tests
- Task 5: minor (deferred): overflow retry resends an identical request when `compact_for_overflow` changed nothing (one extra 400); tool-def Vec cloned each step (`READ_ONLY_DEFS` is now a `LazyLock` and `offered_tools` was inlined), `message_char_len` re-serializes arguments each step; test gaps: `ToolsUnsupported` on the overflow retry, NoTools + emitted tool calls, real concurrency of buffered tools
- Task 5: minor (deferred, live check): providers' acceptance of the wrap-up history (read-only tool calls in history while only the submit tool is offered) on Anthropic and Gemini
- Task 6: minor (deferred): neutralization test never injects `</rules>` into a rule; `skip_note` (pre-Finder manifest reasons) and `skip_reason_label` (summary reasons) use string literals rather than a closed `SkipReason` enum; repo instructions alone never trigger the Rules specialist in Specialists mode (needs a spec change, spec §4.7)
- Task 7: minor (deferred): ~~`read_file_snippet` hand-builds a partial `Toolbox` (free fn over the `Arc`s)~~ (resolved in post-implementation tidy-up); test gaps: cap boundary Critical vs Warning, a second in-flight `ToolsUnsupported`, Rules exemption prompt text; ~~flaky `discover_models` server test on shared Redis~~ (consolidated into `discover_models_never_sends_a_stored_key_to_a_new_base_url` and `discover_models_caches_per_key_and_supports_refresh`)
- Task 8: minor (deferred): move `collect_candidates`/`all_failed`/`finder_cache_tokens` from `engine.rs` to `finder.rs`; test gaps: dedup deadline, `truncated_by_cap` note, `ToolsUnsupported` discarding a sibling success, git 2.30 Warn; `FakeChat` in-flight count leaks on a deadline cut
- Task 8: minor (deferred, live check): Docker image git runtime and `GitCliCloner` end to end against a real provider
- Task 9: minor (deferred): no test for PUT `/settings` with `review_mode: null`; test helper `unwrap`; Indonesian doc comments
- Task 10: minor (deferred): `ReviewMode` TS type defined in two pages (move to a shared `#lib` type); no in-flight guard on the review-mode Select
- Task 11: minor (deferred): a tiny follow-up push to a large PR counts as small (add one sentence to spec §4.13 or decide); settings helper text says "one Generalist pass" while it is one call per batch; settings skeleton lacks the helper-text lines (small layout jump)
- Task 12: minor (deferred): refactor the duplicated diff walkers (fix-wave C-7; `small_pr::changed_lines` was unified with `prompt::changed_line_counts` in the post-implementation tidy-up); the Verifier context starts ~40 lines before `start_line` with a 200-line / 8,000-char read cap, so on a very long span the finding's own line may fall outside it (the tool-enabled Verifier can still read it)
- Task 13: minor (deferred): no Verifier-deadline wiring test, no nudge-after-time-wrap-up test, no NoTools time wrap-up test
- Task 13: minor (deferred): reclaim after 35 min slows dead-worker takeover. Idea: the running worker refreshes its entry's idle time on each heartbeat (XCLAIM … JUSTID to itself), so RECLAIM_IDLE can go back to short.
- Task 13: minor (deferred): `LOCK_TTL` = `RECLAIM_IDLE` = 35 min; after a crash, a reclaim at exactly 35 min may still find the lock held for milliseconds, which costs one 30 s lock-busy retry (attempt unchanged, no double processing)
- Task 14: minor (deferred): `deduplicate_findings` takes `Option<&TokenBudget>` while the others take `Option<&Arc<TokenBudget>>` (in the post-implementation tidy-up `verifier::verify_diff_only` and `dedup::deduplicate_findings` share `agent::chat_low_effort` taking `Option<&TokenBudget>`); settings label "(0-100000000)" is hard to read
- Task 14: minor (deferred): the token ceiling is per run, so a Generalist review retried for `AllKeysFailed` (up to `MAX_ATTEMPTS`) may spend up to the ceiling on each attempt
- Task 14: minor (deferred): ~~the usage page types `cache_write_tokens` but does not show it (trim the type or add a column)~~ (resolved in post-implementation tidy-up: trimmed from `Daily` and `Row` types in `web/src/routes/usage/+page.svelte`)
- Task 15: minor (deferred): plan-mandated false positives (`x-api-secret` header, i18n keys like `forgot_password_hint`); false negatives: typed declarations (`password: str = "…"`), escaped quotes, YAML/compose unquoted (`POSTGRES_PASSWORD: value`)
- Task 15: minor (deferred): grep's 500-char cut runs before redaction and can show a partial token; a one-line double PEM masks only the first; the redacted-output counter double-counts on a concurrent cache miss (debug only); engine redaction runs on the async thread (tiny bodies)
- Task 15: minor (deferred): accepted trade-offs of the narrowed rules: `.env` values ending in `,`/`)` or containing `(` are not masked; indented env lines (YAML `- POSTGRES_PASSWORD=…`) are not masked; one base64-looking word after a quoted BEGIN is still masked
- Final review #1: minor (deferred): `RepoWorkspace` drops its `TempDir` on an async worker thread (hand `remove_dir_all` to `spawn_blocking`); the PR manifest is uncapped and repeated in every batch/step (`compact` never shrinks `messages[0]`); no summary note when `clone_spec` is `None` (e.g. Azure repo without a project GUID); repository instructions are read from the PR head (consider the base commit); watch `verifier_dropped` before tuning `VERIFY_CONCURRENCY`
- Final review #1 / fix-wave re-review: minor (deferred): `workspace.rs` doc comment not rewrapped
- Provider models (merged plan): minor (deferred): bare `"thinking"` marker triggers the no-effort retry too broadly (narrow it, add a negative test); every call to a non-reasoning model costs one rejected request first (no per-model memory); `provider_error` echoes provider text (≤ 200 chars, never the key or URL) and a decrypt error maps to 502; provider + key create not atomic; edit dialog needs a live test; one failed request blanks the Repositories page; test gaps

## PR discussion awareness & learning (plan 2026-11-23)

- Azure thread links: `PrThread.url` is always `None` on Azure, so learned memories have no "View thread" link (needs the repo web URL to build one)
- Engine deadline tests in `crates/review/tests/engine.rs` flake under full-workspace `-j 2` load (pass solo); widen their margins
- Learned-memory inserts and the `pr_learning_runs` row are not one transaction: a DB failure between them can duplicate up to 5 memories on retry
- Semantic Dedup's `already_raised` citation is not checked against the finding's file (only the prompt-level data guard stops an injected thread from suppressing an unrelated finding)
- Memory prompt injection has no cap or relevance selection, so prompts grow with the number of active memories
