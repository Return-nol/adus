# Backlog — deferred minor findings

Collected from the per-task and final reviews of the v1 plan and the follow-up plan (ignored files + Azure project names). None blocked merge. Fix opportunistically.

## v1 plan
- Task 4: minor (deferred): AdminUser untested until first used (Task 5)
- Task 4: minor (deferred): body-too-large maps to 400 not 413
- Task 5: minor (deferred): session delete runs after commit (500 after applied change on failure); missing 403/401 tests for POST/PATCH users and /me*
- Task 5: minor (deferred): collapsed whitespace in UPDATE literal crates/db/src/users.rs:141
- Task 6: minor (deferred): click-through UI never browser-verified — user to test manually; loadSession error invisible on /login
- Task 7: minor (deferred): PullRequest/FileDiff/InlineComment/FileStatus lack serde derives — add when a task serializes them
- Task 8: minor (deferred): HTTP redirects (301 POST→GET on renamed repo) depend on caller's reqwest client — configure redirect policy when building client (Task 23 wiring)
- Task 9: minor (deferred): Azure post_review stops at first failed thread (dup on retry — see Task 21 note); 302-to-signin surfaces as Decode; no content size cap; Azure webhook basic-auth consumerInputs unused (URL token instead)
- Task 10: minor (deferred): PATCH token+name not atomic; set_webhook_ids error in sync_webhooks aborts loop (should become webhook_error); Unauthorized from ensure_webhooks after rotation marks connection invalid (ambiguous / stale resync race); add-repo half-built window; connection delete hard-deletes soft-deleted repos (revisit when reviews FK lands, Task 21); add-repo vs connection delete race → 500; webhook_token stored plaintext (plan-mandated TEXT; consider Cipher); N+1 repo count
- Task 11: minor (deferred): PAT stays in connection-dialog state after failed submit/cancel until reopen (clear on close); first-load failure shows empty-state text; multi-line toast collapses; no busy guard on resync/toggles; failed picks cleared; alert action label always Delete
- Task 12: minor (deferred): provider error.message forwarded verbatim (compatible server could echo key); Gemini model unescaped in URL path
- Task 13: minor (deferred): wrong ADUS_SECRET_KEY marks every key invalid permanently (document in ops docs/README); candidate_keys+get_provider not in one tx
- Task 14: minor (deferred): NULL repository (key-test pings) labelled '(deleted)' in breakdown — consider '(no repository)'; no test for upstream 4xx echoing key; no TZ-shifted session test; provider_in_use+delete not atomic; N+1 keys per provider
- Task 15: minor (deferred): key value kept in state after failed save until close; busy.delete outside finally; fail() duplicated in 4 pages (move to api.ts); chart colors near-gray; Select value='' None unverified at runtime; sidebar /token-usage→/usage fixed
- Task 16: minor (deferred): no test asserts updated_at advances; ACTIVE_REPO const duplicated in rules.rs/memories.rs; update/delete by id don't check active repo; rule_matches_path compiles glob per call (ponytail)
- Task 17: minor (deferred): no unit test for safeHttpUrl; login page not prettier-formatted (fmt keeps reformatting it — format & commit once); scope dropdown includes disabled repos
- Task 18: minor (deferred): XAUTOCLAIM from 0-0 COUNT 1 scans ~10 PEL entries (fine at 2 workers); ~~jobs >15 min get reclaimed while running~~ (resolved in Task 13: `RECLAIM_IDLE` 35 min > `JOB_TIMEOUT` 31 min, compile-time assert)
- Task 18: minor (deferred): empty XPENDING after claim delivers anyway (at-least-once dup, very unlikely)
- Task 19: minor (deferred): GitHub delivery id unsigned → replay with new id bypasses dedup (accepted; optional sha256(body) key); test cleanup leaks streams on failed assertion; CI must start Redis (Task 27)
- Task 20: minor (deferred): first non-empty array could be an example array; title blank lines → double spaces
- Task 21: minor (deferred): 'no model configured' comment repeats on every push (plan-mandated)
- Task 21: minor (deferred): @adudu review on delete-only PR gives no visible feedback (Skipped row only)
- Task 22: minor (deferred): GitHub email fallback in authorize is dead code/latent risk (plan-mandated; doc it); blockquote escapable with bare \r and re-pings @mentions; 'please review' → Unknown; mentions inside code/quotes trigger; missing-repo path untested
- Task 23: minor (deferred): shutdown race strands a delivery until reclaim (now 35 min, +1 attempt) — alternative is_cancelled check at loop top; ~~reclaim duration not exposed for const assert~~ (resolved in Task 13: `adus_queue::RECLAIM_IDLE`)
- Task 13: minor (deferred): reclaim after 35 min slows dead-worker takeover. Idea: the running worker refreshes its entry's idle time on each heartbeat (XCLAIM … JUSTID to itself), so RECLAIM_IDLE can go back to short.
- Task 23: minor (deferred): Azure timeout mid-post → Retryable dup threads (pre-existing; split prepare/post phases, Fatal after posting starts on Azure); panic inside review_pr leaves row running; server failure exits code 0; abort() not awaited before unlock; rows left running after 30 s shutdown grace
- Task 24: minor (deferred): repo filter omits soft-deleted repos; filter change flashes empty table
- Task 25: minor (deferred): localhost warning misses [::1]; check_git list-error branch untested
- Task 26: minor (deferred): env_file passes POSTGRES_PASSWORD/COMPOSE_PROFILES into adus container; interactive prompt + cold build untested
- Task 26: minor (deferred): README !override snippet needs Compose 2.24+; empty compose --short passes check silently
- Task 27: minor (deferred): release.yml applies latest to pre-release tags (drop type=raw latest, rely on flavor latest=auto); release doesn't gate on CI; arm64 via qemu slow (web stage --platform=$BUILDPLATFORM, timeout-minutes)

## Follow-up plan
- Task 1: minor (deferred): reviews API repository.name now carries display name (misleading field name)
- Task 2: minor (deferred): unused name field in repositories page Repo type; no 'N selected' hint for hidden selections
- Task 3: minor (deferred): GlobSet build failure mapped to InvalidGlob(message); settings replace read-then-write not transactional
- Task 4: minor (deferred): TooLong/TooMany pattern + >500 keywords + [] keywords branches untested; settings PUT read-then-write race
- Task 5: minor (deferred): ignored_title label in prompt.rs never displayed
- Task 6: minor (deferred): Reset disabled while unsaved edits on default list; confirm action no busy guard; keywords not validated client-side
- Task 7: minor (deferred): 15-min stale threshold hard-coded in SQL (tie to const + assert); fail_stale inside select arm stalls promote during DB outage; 'interrupted (worker stopped)' text misleading for live sweeps
- fail_stale SQL string is one long line (cosmetic)
