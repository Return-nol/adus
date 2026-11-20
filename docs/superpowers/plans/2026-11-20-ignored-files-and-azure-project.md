# Ignored files + Azure project names Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show Azure repos as `project/repo` (with search in the add-repo dialog), add a global, admin-editable ignored-files list (Kodus defaults) plus ignored title keywords, and land three small v1 follow-ups.

**Architecture:** One new column per concern (`repositories.project_name`, `settings.ignore_paths`, `settings.ignored_title_keywords`). The server computes `display_name` once and every page uses it. Ignore matching is a `GlobSet` built once per review in the engine, filtering files before any LLM call.

**Tech Stack:** Rust (axum, sqlx runtime queries, globset), SvelteKit 3 + Svelte 5 + shadcn-svelte (DESIGN.md), Postgres.

**Spec:** `docs/superpowers/specs/2026-11-20-ignored-files-and-azure-project-design.md` (also binding: v1 spec, AGENTS.md, DESIGN.md)

## Global Constraints

- All AGENTS.md conventions apply (ApiJson/ApiPath/ApiQuery, error JSON, runtime sqlx, `#lib` imports, DESIGN.md tokens, no browser automation, `cargo test --workspace -j 2` on Windows).
- `display_name`: Azure `project_name/name`, falling back to `owner/name` when `project_name` is NULL; GitHub `owner/name`. Computed on the server only.
- Default ignore list = Kodus `default-kodus-config.yml` `ignorePaths`, verbatim, with an AGPL attribution comment. `settings.ignore_paths = NULL` means "use defaults"; reset = set NULL.
- Ignore matching uses the same globset semantics as rules (`globset::Glob::new` defaults, `*` crosses `/`) on `normalize_path(path)`.
- Ignored files are skipped before the LLM, are not counted toward `max_files_per_review`, and add one summary line `N files ignored`. They are not listed by name.
- Pattern validation: trimmed, empty and duplicates dropped, each ≤ 500 chars, ≤ 5,000 patterns, invalid → 422 `invalid_glob` naming the pattern. Title keyword ≤ 100 chars.
- A title keyword match (case-insensitive substring) → review `skipped` with reason `ignored_title`, no comment. Trigger `Command` bypasses it.
- Model choice: Tasks 5 and 7 touch the review engine/worker → implementer model **opus, effort high**. The other tasks use **sonnet, effort high**.

## Review Focus

1. **Existing Azure repos added before this change** have `project_name` NULL. They must show `org/name` (fallback) until backfilled, and be backfilled when the remote-repos list opens or webhooks resync. Test in Task 1.
2. **An admin saves an ignore list containing one bad glob** among 1,100 → 422 naming that pattern, and nothing is saved. Test in Task 4.
3. **An ignore pattern matches every changed file** → the review is `skipped` (`nothing_to_review`), not `done` with an empty summary, and no LLM call is made. Test in Task 5.
4. **The incremental review path** must apply ignore too, not only full reviews. Test in Task 5.
5. **`@adudu review` on a PR whose title has an ignored keyword** still reviews. An automatic `opened`/`updated` run on it is skipped. Test in Task 5.

---

### Task 1: Azure project name + `display_name` (backend)

**Files:**
- Create: `migrations/0006_repository_project_name.sql` (`ALTER TABLE repositories ADD COLUMN project_name text NULL`)
- Modify: `crates/git/src/types.rs` (`RemoteRepo`), `crates/git/src/azure.rs` (`AzProject.name`), `crates/git/src/github.rs` (`project_name: None`), `crates/db/src/repositories.rs`, `crates/db/src/reviews.rs` (history repository label), `crates/db/src/usage.rs` (repository breakdown label), `crates/server/src/api/repositories.rs`, `crates/server/src/api/connections.rs` (remote-repos + resync backfill), all `RemoteRepo` constructions in test fakes
- Test: `crates/git/tests/azure.rs`, `crates/db` tests, `crates/server/tests/connections.rs`, `crates/server/tests/reviews.rs`, `crates/server/tests/usage.rs`

**Interfaces:**
- Produces:
  - `RemoteRepo { r#ref: RepoRef, default_branch: Option<String>, project_name: Option<String> }`
  - `Repository.project_name: Option<String>` and `NewRepository.project_name: Option<&str>`
  - `repositories::display_name(repo: &Repository) -> String`
  - `repositories::display_name_sql(alias: &str) -> String`, returning `COALESCE({alias}.project_name, {alias}.owner) || '/' || {alias}.name` (used by reviews history and usage breakdown so the label rule lives in one place)
  - `repositories::set_project_name(pool, connection_id: Uuid, external_id: &str, project_name: &str) -> Result<()>` (active rows only, no-op when unchanged)
  - `RepositoryJson` gains `project_name` and `display_name`
  - remote-repos JSON items gain `display_name` (same rule; GitHub `owner/name`)
  - reviews `repository.name` and the usage repository label both become the display name

- [ ] **Step 1: Write the failing tests**
  - `azure list_repos_reads_project_name`: fixture project `{id, name:"myvalue"}` → `project_name == Some("myvalue")`.
  - db `display_name_rules`: Azure with project_name → `"myvalue/api"`; Azure without → `"ovalteam/api"`; GitHub → `"octo/hello"`.
  - server `remote_repos_backfills_project_name`: an Azure repo inserted with `project_name` NULL. After `GET /connections/{id}/remote-repos`, the row has `project_name == "myvalue"` and `GET /repositories` shows `display_name == "myvalue/api"`.
  - server `resync_backfills_project_name`: same, via `POST /connections/{id}/resync-webhooks`.
  - reviews and usage tests: the repository label equals `"myvalue/api"` for an Azure repo with a project name.
- [ ] **Step 2:** Run the tests → FAIL.
- [ ] **Step 3:** Implement. On add-repo, store `project_name` from the chosen `RemoteRepo`.
- [ ] **Step 4:** `cargo test -j 2 -p adus-git -p adus-db -p adus-server` → PASS.
- [ ] **Step 5: Commit** `feat: Azure project names and display_name for repositories`

### Task 2: Use `display_name` in the dashboard + add-repo search

**Files:**
- Modify: `web/src/lib/components/add-repo-dialog.svelte`, `web/src/routes/repositories/+page.svelte`, `web/src/routes/reviews/+page.svelte`, `web/src/routes/rules/+page.svelte`, and any other `${owner}/${name}` label (`grep -rn "owner}/" web/src`)

**Interfaces:**
- Consumes: the Task 1 JSON fields `display_name` on repositories and remote repos.
- Add-repo dialog:
  - A search `Input` (placeholder "Search repositories"), filtering case-insensitively on `display_name`.
  - The list is sorted by `display_name`.
  - A counter `"{shown} of {total} repositories"`.
  - Search does not clear checked selections that are filtered out of view.
  - The empty search result shows "No repositories match".

- [ ] **Step 1:** Replace every client-built `owner/name` label with `display_name` and add the search UI.
- [ ] **Step 2:** `pnpm -C web fmt && pnpm -C web check` (0 errors) and `pnpm -C web build`. `grep -rn "owner}/" web/src` returns nothing.
- [ ] **Step 3: Commit** `feat(web): show project/repo names and search in add repositories`

### Task 3: Ignore matcher + defaults + settings storage

**Files:**
- Create: `migrations/0007_ignore_settings.sql` (`ALTER TABLE settings ADD COLUMN ignore_paths text[] NULL, ADD COLUMN ignored_title_keywords text[] NOT NULL DEFAULT '{}'`)
- Create: `crates/core/src/default_ignore.rs`, holding `pub const DEFAULT_IGNORE_PATHS: &[&str]`. Copy it verbatim from the `ignorePaths:` list of Kodus `default-kodus-config.yml` (https://github.com/kodustech/kodus-ai, AGPL-3.0; a local copy may exist under the session scratchpad). Put the attribution comment at the top. Do NOT copy anything from `libs/ee/**` or `*.ee.*`.
- Create: `crates/db/src/ignore.rs`
- Modify: `crates/db/src/settings.rs` (`Settings` gains `ignore_paths: Option<Vec<String>>` and `ignored_title_keywords: Vec<String>`)
- Test: `crates/db/src/ignore.rs` (mod tests), the db settings test

**Interfaces:**
- Produces:
  - `adus_core::default_ignore::DEFAULT_IGNORE_PATHS`
  - `ignore::effective_ignore_paths(settings: &Settings) -> Vec<String>`, returning the stored list or the defaults
  - `ignore::normalize_patterns(raw: &[String]) -> Result<Vec<String>, IgnoreError>`, which trims, drops empty entries and duplicates (keeping order), and validates. `IgnoreError::{InvalidGlob(String), TooLong(String), TooMany}`.
  - `ignore::IgnoreMatcher::new(patterns: &[String]) -> Result<IgnoreMatcher, IgnoreError>`
  - `IgnoreMatcher::matching_pattern(&self, path: &str) -> Option<&str>`, which works on `normalize_path`
  - `IgnoreMatcher::is_ignored(&self, path: &str) -> bool`
  - `ignore::title_matches(title: &str, keywords: &[String]) -> bool`, a case-insensitive substring check that ignores empty keywords

- [ ] **Step 1: Write the failing tests**
  - `defaults_load_and_compile`: `DEFAULT_IGNORE_PATHS.len() > 1000`, and `IgnoreMatcher::new` on the defaults is `Ok`.
  - `defaults_ignore_lock_and_media`: `yarn.lock`, `package-lock.json`, `assets/logo.png` and `/src/a.json` are ignored; `src/main.rs` is not.
  - `matching_pattern_reports_pattern`: patterns `["dist/**"]` and path `dist/a.js` → `Some("dist/**")`.
  - `normalize_rejects_bad_glob`: `["ok/**", "a[b"]` → `InvalidGlob("a[b")`.
  - `normalize_trims_dedups_drops_empty`.
  - `normalize_limits`: a 501-char pattern → `TooLong`; 5,001 patterns → `TooMany`.
  - `title_matches_case_insensitive`: `"WIP: x"` with `["wip"]` → true; `[""]` → false.
  - db `settings_ignore_paths_null_means_defaults`: a fresh DB has `ignore_paths == None`, and `effective_ignore_paths` equals the defaults.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -j 2 -p adus-core -p adus-db` → PASS.
- [ ] **Step 5: Commit** `feat: global ignore list with Kodus defaults`

### Task 4: Settings API for ignore + test-a-file

**Files:**
- Modify: `crates/server/src/api/settings.rs`
- Test: `crates/server/tests/settings_ignore.rs`

**Interfaces:**
- Consumes: Task 3.
- `GET /api/v1/settings` adds `ignore_paths: string[]` (effective), `ignore_paths_is_default: bool` and `ignored_title_keywords: string[]`.
- `PUT /api/v1/settings` (admin) accepts optional `ignore_paths: string[] | null` and `ignored_title_keywords: string[]`. Missing means unchanged; `null` means reset to defaults (store NULL). Errors are 422 `invalid_glob` with the pattern in the message, or 422 `validation_error` for length or count. A save with any error stores nothing.
- `POST /api/v1/settings/ignore-test {path}` (any logged-in user) returns `{ignored: bool, pattern: string|null}` against the effective list.

- [ ] **Step 1: Write the failing tests**
  - `get_settings_returns_defaults`: `ignore_paths_is_default == true`.
  - `put_custom_list_then_reset`.
  - `put_with_one_bad_glob_saves_nothing`: Review Focus 2. Send the defaults plus `"a[b"` → 422, and the list is unchanged.
  - `title_keyword_too_long_422`.
  - `ignore_test_reports_pattern`.
  - `user_role_cannot_put_ignore_403`.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -j 2 -p adus-server --test settings_ignore` → PASS.
- [ ] **Step 5: Commit** `feat(server): ignore paths and title keywords in settings`

### Task 5: Engine applies ignore + title keywords (opus high)

**Files:**
- Modify: `crates/review/src/engine.rs`, `crates/review/src/prompt.rs` (`format_summary` gains `ignored: usize`; label for `ignored_title`)
- Test: `crates/review/tests/engine.rs`, the `prompt.rs` tests

**Interfaces:**
- Consumes: Task 3 (`IgnoreMatcher`, `effective_ignore_paths`, `title_matches`).
- `format_summary(counts, skipped, ignored: usize) -> String`. When `ignored > 0` it adds the line `"{ignored} files ignored"` (singular `"1 file ignored"`) after the counts.
- `run` behaviour:
  - When trigger ≠ `Command` and `title_matches(pr.title, keywords)` → `Outcome::Skipped("ignored_title")`. This check happens before any diff fetch.
  - Build `IgnoreMatcher` once from `effective_ignore_paths`. If the stored list fails to compile, log a warning and fall back to the defaults (never fail the review).
  - In the candidate loop, an ignored file is checked first: count it, do not push it to targets or skipped, and do not count it toward the file limit.
  - If no targets remain and nothing was skipped → `Skipped("nothing_to_review")`, even when files were ignored.

- [ ] **Step 1: Write the failing tests**
  - `ignored_files_not_sent_to_llm`: patterns `["**/*.json"]`, PR with `a.json` and `b.rs` → exactly one chat call (for `b.rs`), and the summary contains `"1 file ignored"`.
  - `ignored_files_do_not_count_toward_file_limit`: `max_files_per_review = 1`, with `a.json` and `b.rs` → `b.rs` is reviewed and no `file_limit` skip appears.
  - `all_files_ignored_is_skipped_nothing_to_review`: Review Focus 3. Zero chat calls, nothing posted.
  - `incremental_review_applies_ignore`: Review Focus 4.
  - `ignored_title_skips_auto_but_not_command`: Review Focus 5.
  - prompt test `summary_includes_ignored_count`.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -j 2 -p adus-review` → PASS.
- [ ] **Step 5: Commit** `feat(review): skip ignored files and ignored PR titles`

### Task 6: Settings page — Ignored files UI

**Files:**
- Modify: `web/src/routes/settings/+page.svelte`. A child component under `web/src/lib/components/` is allowed if the page grows past ~300 lines.

**Interfaces:**
- Consumes: Task 4 endpoints.
- The **Ignored files** section (DESIGN.md card) contains:
  - A monospace `Textarea`, one pattern per line, with the pattern count.
  - A "Using defaults" badge when `ignore_paths_is_default`.
  - **Reset to defaults**, which sends `ignore_paths: null`.
  - Server errors shown inline (the `invalid_glob` message names the bad pattern).
  - **Test a file**: an input plus a button calling `ignore-test`, showing `Ignored by <code>pattern</code>` or `Will be reviewed`.
- The **Ignored title keywords** field is a textarea, one keyword per line.
- Save uses the existing settings Save flow and busy guard. Non-admins see the existing admin-only card.

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** `pnpm -C web fmt && pnpm -C web check` (0 errors) and `pnpm -C web build`.
- [ ] **Step 3: Commit** `feat(web): ignored files and title keywords in settings`

### Task 7: v1 follow-ups from the final review (opus high)

**Files:**
- Modify: `crates/review/src/engine.rs`, `crates/adus/src/worker.rs` (`run_scheduler`), `crates/llm/src/router.rs` (`ChatError::Internal` display)
- Test: `crates/review/tests/engine.rs`, `crates/adus/tests/worker.rs`, `crates/llm/tests/router.rs`

**Interfaces:**
- When no model is configured and the trigger is `Updated` → `Ok(Outcome::Skipped("no_model"))`: no comment, no failed row, no dead letter. `Opened` and `Command` keep the current behaviour (comment + Fatal). Add a label for `no_model`.
- `run_scheduler(queue, db: PgPool, shutdown)` also runs `reviews::fail_stale` every 5 minutes, logging the count when > 0. Update the call in `main.rs`.
- `ChatError::Internal` Display becomes `"internal error: {0}"`. Update any test asserting the old text.

- [ ] **Step 1: Write the failing tests**
  - `no_model_on_updated_is_skipped_not_failed`: row status `skipped`, `skip_reason == "no_model"`, nothing posted.
  - `scheduler_sweeps_stale_reviews`: use a short interval via a parameter, or call the sweep helper directly.
  - `internal_error_display_text`.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -j 2 --workspace` → PASS. Clippy and fmt are clean.
- [ ] **Step 5:** Update the AGENTS.md lines for no-model handling and the stale sweep, and add the ignore-list and `display_name` rules to Code conventions.
- [ ] **Step 6: Commit** `fix: no-model skip on updates, periodic stale sweep, clearer internal error`
