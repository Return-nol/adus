//! `adus doctor` dan `GET /api/v1/admin/doctor`: apakah review benar-benar bisa jalan?
//!
//! Hanya membaca. Tidak memanggil `ensure_webhooks`, tidak menulis status connection, dan
//! tidak pernah memuat rahasia: yang muncul hanya nama connection, repo, dan provider.
//! Satu-satunya tulisan ke luar adalah `XGROUP CREATE ... MKSTREAM` yang idempoten di
//! `Queue::connect` (saat CLI membangun state); `run` sendiri tidak menulis apa pun.

use std::time::Duration;

use adus_db::{ai, connections, repositories, settings};
use adus_review::workspace::{parse_git_version, version_supported};
use serde::Serialize;

use crate::{
    api::{connections::describe, repositories::repo_ref},
    state::AppState,
};

/// Satu panggilan ke provider git yang lebih lama dari ini dianggap gagal.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);
/// `git --version` yang lebih lama dari ini dianggap gagal.
const GIT_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Urutan deklarasi = urutan tampil: Fail dulu, lalu Warn, lalu Ok.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Fail,
    Warn,
    Ok,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    /// Apa akibatnya bagi review. Kosong jika `Ok`.
    pub impact: Option<String>,
    /// Apa yang harus dilakukan. Kosong jika `Ok`.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ok,
    Degraded,
    NotRunning,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub verdict: Verdict,
    pub checks: Vec<Check>,
}

impl Check {
    fn ok(name: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status: Status::Ok,
            impact: None,
            fix: None,
        }
    }

    fn problem(status: Status, name: impl Into<String>, impact: &str, fix: &str) -> Check {
        Check {
            name: name.into(),
            status,
            impact: Some(impact.to_string()),
            fix: Some(fix.to_string()),
        }
    }
}

impl Report {
    /// Laporan untuk keadaan yang bahkan tidak bisa memulai pengecekan (CLI saja).
    pub fn cannot_start(name: &str, fix: &str) -> Report {
        let impact = "Adus cannot run, so no review can happen";
        Report {
            verdict: Verdict::NotRunning,
            checks: vec![Check::problem(Status::Fail, name, impact, fix)],
        }
    }
}

/// Jalankan semua pengecekan. Cek inti (Postgres, migrasi, Redis, worker, model, key) yang
/// gagal berarti `NotRunning`; token connection dan webhook repo yang gagal berarti `Degraded`.
pub async fn run(state: &AppState) -> Report {
    let mut core = Vec::new();
    let mut extra = Vec::new();

    if check_database(state, &mut core).await {
        check_settings(state, &mut core).await;
    }
    check_redis(state, &mut core, &mut extra).await;
    check_git(state, &mut extra).await;
    check_git_binary(&mut extra).await;

    let verdict = if core.iter().any(|c| c.status == Status::Fail) {
        Verdict::NotRunning
    } else if extra.iter().any(|c| c.status == Status::Fail) {
        Verdict::Degraded
    } else {
        Verdict::Ok
    };
    let mut checks = core;
    checks.extend(extra);
    checks.sort_by_key(|c| c.status); // stabil: urutan pengecekan tetap dalam tiap status
    Report { verdict, checks }
}

/// Postgres terjangkau dan semua migrasi terpasang. `false` jika DB tidak bisa dipakai,
/// supaya cek yang bergantung padanya dilewati (bukan menambah kegagalan ganda).
async fn check_database(state: &AppState, out: &mut Vec<Check>) -> bool {
    if sqlx::query("SELECT 1").execute(&state.db).await.is_err() {
        out.push(Check::problem(
            Status::Fail,
            "Database",
            "Nothing can be stored or read, so reviews cannot run",
            "Make sure Postgres is running and DATABASE_URL is correct",
        ));
        return false;
    }
    out.push(Check::ok("Database"));
    match adus_db::unapplied_migrations(&state.db).await {
        Ok(missing) if missing.is_empty() => out.push(Check::ok("Database migrations")),
        Ok(missing) => out.push(Check::problem(
            Status::Fail,
            "Database migrations",
            &format!(
                "{} migration(s) are not applied; the schema does not match this version",
                missing.len()
            ),
            "Start `adus` (it applies migrations on start). If it fails, inspect the failed migration in the _sqlx_migrations table",
        )),
        Err(_) => out.push(Check::problem(
            Status::Fail,
            "Database migrations",
            "The migration history cannot be read",
            "Start `adus` once so it can create and apply the migrations",
        )),
    }
    true
}

/// Provider default terisi (aktif + punya primary_model) dan punya minimal satu key yang bisa dipakai.
async fn check_settings(state: &AppState, out: &mut Vec<Check>) {
    let Ok(settings) = settings::get(&state.db).await else {
        out.push(Check::problem(
            Status::Fail,
            "Default provider",
            "Settings cannot be read, so no provider can be chosen",
            "Check the database and run `adus doctor` again",
        ));
        return;
    };
    let Some(provider_id) = settings.default_provider_id else {
        out.push(Check::problem(
            Status::Fail,
            "Default provider",
            "No default AI provider is set, so reviews have no model to use",
            "Open Settings and choose a default provider",
        ));
        return;
    };
    let provider = ai::get_provider(&state.db, provider_id)
        .await
        .ok()
        .flatten();
    let Some(provider) = provider else {
        out.push(Check::problem(
            Status::Fail,
            "Default provider",
            "The configured default provider could not be loaded",
            "Open Settings and choose a valid default provider",
        ));
        return;
    };
    let primary = provider.primary_model().filter(|_| provider.enabled);
    let fast = provider
        .fast_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match (primary, fast) {
        (None, _) => out.push(Check::problem(
            Status::Fail,
            "Default provider",
            &format!(
                "{} is disabled or has no primary model configured, so reviews cannot run",
                provider.name
            ),
            "Open AI Providers to enable the provider and configure its primary model",
        )),
        (Some(primary), Some(fast)) => out.push(Check::ok(format!(
            "Default provider: {} ({primary} / {fast})",
            provider.name
        ))),
        (Some(primary), None) => out.push(Check::ok(format!(
            "Default provider: {} ({primary})",
            provider.name
        ))),
    }

    let usable = ai::candidate_keys(&state.db, provider_id, chrono::Utc::now())
        .await
        .map(|keys| keys.len());
    match usable {
        Ok(n) if n > 0 => out.push(Check::ok(format!("AI key: {} ({n} usable)", provider.name))),
        _ => out.push(Check::problem(
            Status::Fail,
            "AI key",
            &format!(
                "{} has no enabled key that is valid and out of cooldown, so every review fails",
                provider.name
            ),
            "Open AI Providers and add a key, or fix or enable an existing one",
        )),
    }
}

/// Redis terjangkau, ada worker hidup, dan stream dead-letter kosong (Warn jika tidak).
async fn check_redis(state: &AppState, core: &mut Vec<Check>, extra: &mut Vec<Check>) {
    match state.queue.live_workers().await {
        Err(_) => core.push(Check::problem(
            Status::Fail,
            "Redis",
            "Jobs cannot be queued, so webhooks cannot start reviews",
            "Make sure Redis is running and REDIS_URL is correct",
        )),
        Ok(workers) => {
            core.push(Check::ok("Redis"));
            if workers.is_empty() {
                core.push(Check::problem(
                    Status::Fail,
                    "Workers",
                    "Jobs are queued but nothing processes them, so no review is posted",
                    "Start `adus serve` (ADUS_WORKERS must be at least 1) and check its logs",
                ));
            } else {
                core.push(Check::ok(format!("Workers: {} running", workers.len())));
            }
            if let Ok(dead) = state.queue.dead_count().await
                && dead > 0
            {
                extra.push(Check::problem(
                    Status::Warn,
                    format!("{dead} failed job(s) in the dead-letter stream"),
                    "These pull requests did not get a review",
                    "Look at `XRANGE adus:jobs:dead - +` in Redis for the error, fix the cause, then re-trigger with `@adudu review`",
                ));
            }
        }
    }
}

/// Token tiap connection dan webhook tiap repo aktif, lewat panggilan baca-saja ke provider.
async fn check_git(state: &AppState, out: &mut Vec<Check>) {
    let (Ok(conns), Ok(repos)) = (
        connections::list(&state.db).await,
        repositories::list(&state.db, None).await,
    ) else {
        out.push(Check::problem(
            Status::Fail,
            "Connections and repositories",
            "The connections and repositories cannot be read, so their tokens and webhooks were not checked",
            "Check the database and run the checks again",
        ));
        return;
    };
    // ponytail: berurutan; N connection + M repo × maks 15 dtk. Batasi concurrency jika daftarnya panjang.
    for conn in conns {
        let name = format!("Connection: {}", conn.name);
        let Ok(token) = state.cipher.decrypt(&conn.encrypted_token) else {
            out.push(Check::problem(
                Status::Fail,
                name,
                "The stored token cannot be decrypted, so its repositories cannot be reviewed",
                "Check that ADUS_SECRET_KEY is the key the token was saved with, or rotate the token",
            ));
            continue;
        };
        let provider = state.git.create(conn.kind, &conn.org_or_owner, &token);
        let verified = tokio::time::timeout(PROVIDER_TIMEOUT, provider.verify()).await;
        let problem = match verified {
            Ok(Ok(_)) => None,
            Ok(Err(err)) => Some(describe(&err)),
            Err(_) => Some("the request timed out"),
        };
        if let Some(reason) = problem {
            out.push(Check::problem(
                Status::Fail,
                name,
                &format!(
                    "Adus cannot read pull requests or post reviews for this connection: {reason}"
                ),
                "Open Repositories and rotate the token for this connection",
            ));
            continue; // repo-nya tidak diperiksa: akan gagal dengan sebab yang sama
        }
        out.push(Check::ok(name));

        for repo in repos
            .iter()
            .filter(|r| r.connection_id == conn.id && r.enabled)
        {
            let name = format!("Webhooks: {}", repositories::display_name(repo));
            let found = if repo.webhook_external_ids.is_empty() {
                Ok(Ok(false))
            } else {
                // URL yang sekarang: hook yang masih mengarah ke ADUS_PUBLIC_URL lama dianggap hilang.
                let url = repositories::webhook_url(&state.config.public_url, repo, conn.kind);
                tokio::time::timeout(
                    PROVIDER_TIMEOUT,
                    provider.webhooks_exist(&repo_ref(repo), &repo.webhook_external_ids, &url),
                )
                .await
            };
            match found {
                Ok(Ok(true)) => out.push(Check::ok(name)),
                Ok(Ok(false)) => out.push(Check::problem(
                    Status::Fail,
                    name,
                    "New pull requests and `@adudu` commands in this repository are not seen",
                    "Open Repositories and use Resync webhooks on this connection",
                )),
                Ok(Err(err)) => out.push(Check::problem(
                    Status::Fail,
                    name,
                    &format!("The webhooks could not be checked: {}", describe(&err)),
                    "Try again; if it persists, resync webhooks or rotate the token",
                )),
                Err(_) => out.push(Check::problem(
                    Status::Fail,
                    name,
                    "The webhooks could not be checked: the request timed out",
                    "Try again; if it persists, check the network path to the provider",
                )),
            }
        }
    }
}

/// `git` 2.31+ di PATH, untuk clone review. Tanpa itu review tetap jalan dengan diff saja,
/// jadi hanya `Warn`.
async fn check_git_binary(out: &mut Vec<Check>) {
    out.push(git_binary_check("git").await);
}

/// Jalankan `program --version` (tanpa shell, maks 5 dtk): `Ok` untuk git 2.31+, selain itu
/// `Warn`. `program` adalah parameter supaya test bisa memakai binary yang tidak ada.
pub async fn git_binary_check(program: &str) -> Check {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_VERSION_TIMEOUT, command.output()).await;
    let version = match output {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            parse_git_version(&text)
                .filter(|v| version_supported(*v))
                .map(|_| text)
        }
        _ => None,
    };
    match version {
        Some(text) => Check::ok(format!("Git: {text}")),
        None => Check::problem(
            Status::Warn,
            "Git",
            "`git` 2.31+ not found — reviews fall back to diff-only mode without codebase context",
            "Install git 2.31 or newer on the host that runs `adus serve` and make sure it is on PATH (the Docker image already includes it)",
        ),
    }
}

/// Teks untuk terminal: baris pertama vonis, lalu `✘` / `!` / `✓` per cek (Fail lebih dulu).
pub fn print_report(report: &Report) -> String {
    let verdict = match report.verdict {
        Verdict::Ok => "OK",
        Verdict::Degraded => "DEGRADED",
        Verdict::NotRunning => "NOT RUNNING",
    };
    let mut out = format!("Reviews: {verdict}\n");
    for check in &report.checks {
        let icon = match check.status {
            Status::Fail => '✘',
            Status::Warn => '!',
            Status::Ok => '✓',
        };
        out.push_str(&format!("{icon} {}\n", check.name));
        if let Some(impact) = &check.impact {
            out.push_str(&format!("    Impact: {impact}\n"));
        }
        if let Some(fix) = &check.fix {
            out.push_str(&format!("    Fix: {fix}\n"));
        }
    }
    out
}
