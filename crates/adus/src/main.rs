use std::{collections::HashMap, process::ExitCode, sync::Arc, time::Duration};

use adus::{JOB_TIMEOUT, STALE_SWEEP_EVERY, run_scheduler, run_worker};
use adus_core::{
    config::{self, Config, Level},
    crypto::{Cipher, generate_secret_key},
};
use adus_git::{GitProviderFactory, HttpGitProviderFactory};
use adus_llm::router::{ChatService, LlmRouter};
use adus_queue::Queue;
use adus_review::engine::{Engine, REVIEW_TIMEOUT};
use adus_review::workspace::{GitCliCloner, cleanup_orphaned_workspaces};
use adus_server::{
    AppState,
    doctor::{self, Report, Verdict},
    router,
};
use clap::{Parser, Subcommand};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// Setelah sinyal berhenti, job yang sedang jalan diberi waktu selama ini untuk selesai.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Probe `/healthz` dibatasi waktu ini, supaya `HEALTHCHECK` Docker tidak menggantung.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    version,
    about = "Self-hosted AI code review. Without a command, `adus` runs `serve`."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the web server, workers and scheduler (the default)
    Serve,
    /// Print a new random ADUS_SECRET_KEY (needs no environment)
    GenSecret,
    /// Validate the environment variables; exit 1 if any is wrong
    CheckConfig,
    /// Check whether reviews can run (read-only); exit 1 if they cannot
    Doctor,
    /// Probe /healthz on this machine; exit 0 if it answers (for Docker HEALTHCHECK)
    Healthcheck,
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => {
            serve().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::GenSecret => {
            println!("{}", generate_secret_key());
            Ok(ExitCode::SUCCESS)
        }
        Command::CheckConfig => Ok(check_config()),
        Command::Doctor => Ok(doctor().await),
        Command::Healthcheck => Ok(healthcheck().await),
    }
}

/// Semua yang dibutuhkan handler dan worker. `migrate = false` untuk `doctor`, yang tidak boleh
/// mengubah apa pun.
async fn build_state(
    config: Config,
    migrate: bool,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let cipher = Arc::new(Cipher::from_base64_key(&config.secret_key)?);
    let db = if migrate {
        adus_db::connect(&config.database_url).await?
    } else {
        adus_db::connect_without_migrating(&config.database_url).await?
    };
    let queue = Queue::connect(&config.redis_url, "adus").await?;
    // Tanpa redirect: 301 pada POST tidak boleh diam-diam berubah menjadi GET.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let git: Arc<dyn GitProviderFactory> = Arc::new(HttpGitProviderFactory::new(http.clone()));
    let chat: Arc<dyn ChatService> =
        Arc::new(LlmRouter::new(db.clone(), cipher.clone(), http.clone()));
    Ok(AppState {
        db,
        cipher,
        config: Arc::new(config),
        git,
        http,
        queue,
        chat,
    })
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let state = build_state(Config::from_env()?, true).await?;
    // Sebelum worker jalan: review yang tertinggal `queued`/`running` dari proses sebelumnya.
    match adus_db::reviews::fail_stale(&state.db).await {
        Ok(0) => {}
        Ok(count) => tracing::warn!(count, "marked interrupted reviews as failed"),
        Err(err) => tracing::warn!(error = %err, "could not mark interrupted reviews as failed"),
    }
    // Also before workers run: clone directories left behind by a crash.
    let removed = cleanup_orphaned_workspaces().await;
    if removed > 0 {
        tracing::info!(removed, "removed orphaned review workspaces");
    }
    let listener = tokio::net::TcpListener::bind(&state.config.bind_addr).await?;
    tracing::info!("listening on {}", state.config.bind_addr);
    let engine = Arc::new(Engine {
        db: state.db.clone(),
        cipher: state.cipher.clone(),
        git: state.git.clone(),
        chat: state.chat.clone(),
        cloner: Arc::new(GitCliCloner::default()),
        review_timeout: REVIEW_TIMEOUT,
    });

    let shutdown = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".to_string());
    for index in 0..state.config.workers {
        tasks.spawn(run_worker(
            format!("worker-{host}-{index}"),
            state.queue.clone(),
            engine.clone(),
            JOB_TIMEOUT,
            shutdown.clone(),
        ));
    }
    tasks.spawn(run_scheduler(
        state.queue.clone(),
        state.db.clone(),
        STALE_SWEEP_EVERY,
        shutdown.clone(),
    ));
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutting down: no new jobs, waiting for running jobs");
        signal_token.cancel();
    });

    let server_token = shutdown.clone();
    tasks.spawn(async move {
        let server = axum::serve(listener, router(state))
            .with_graceful_shutdown(server_token.clone().cancelled_owned());
        if let Err(err) = server.await {
            tracing::error!(error = %err, "HTTP server failed; shutting down");
            server_token.cancel();
        }
    });

    // Masa tenggang dihitung sejak sinyal berhenti, untuk HTTP dan worker bersama-sama.
    shutdown.cancelled().await;
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while let Some(joined) = tasks.join_next().await {
            if let Err(err) = joined {
                tracing::error!(error = %err, "task failed during shutdown");
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("jobs still running after 30 s; exiting anyway");
    }
    Ok(())
}

fn check_config() -> ExitCode {
    let vars: HashMap<String, String> = std::env::vars().collect();
    let issues = config::check(&vars);
    print!("{}", config::format_issues(&issues));
    if issues.iter().any(|i| i.level == Level::Error) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Keluar 1 hanya jika vonisnya `NOT RUNNING`.
async fn doctor() -> ExitCode {
    // Pesan error mentah tidak dicetak: bisa memuat bagian dari URL koneksi.
    let state = match Config::from_env() {
        Ok(config) => build_state(config, false).await.map_err(|_| {
            Report::cannot_start(
                "Postgres or Redis",
                "Make sure both are running and DATABASE_URL, REDIS_URL and ADUS_SECRET_KEY are correct; `adus check-config` helps",
            )
        }),
        Err(_) => Err(Report::cannot_start(
            "Configuration",
            "Run `adus check-config` and fix what it reports",
        )),
    };
    let report = match state {
        Ok(state) => doctor::run(&state).await,
        Err(report) => report,
    };
    print!("{}", doctor::print_report(&report));
    if report.verdict == Verdict::NotRunning {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Hanya membaca `ADUS_BIND_ADDR` (bawaan `0.0.0.0:8080`) dan memeriksa port itu di 127.0.0.1.
async fn healthcheck() -> ExitCode {
    let bind = std::env::var("ADUS_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let Some((_, port)) = bind.rsplit_once(':') else {
        eprintln!("ADUS_BIND_ADDR must look like host:port");
        return ExitCode::FAILURE;
    };
    // no_proxy: 127.0.0.1 tidak boleh lewat proxy dari HTTP_PROXY.
    let Ok(client) = reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        .no_proxy()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    match client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
    {
        Ok(reply) if reply.status().is_success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// Tunggu Ctrl+C, atau SIGTERM di unix (`docker stop`).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
