use std::{sync::Arc, time::Duration};

use adus::{JOB_TIMEOUT, run_scheduler, run_worker};
use adus_core::{config::Config, crypto::Cipher};
use adus_git::{GitProviderFactory, HttpGitProviderFactory};
use adus_llm::router::{ChatService, LlmRouter};
use adus_queue::Queue;
use adus_review::engine::Engine;
use adus_server::{AppState, router};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// Setelah sinyal berhenti, job yang sedang jalan diberi waktu selama ini untuk selesai.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let cipher = Arc::new(Cipher::from_base64_key(&config.secret_key)?);
    let db = adus_db::connect(&config.database_url).await?;
    let queue = Queue::connect(&config.redis_url, "adus").await?;
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("listening on {}", config.bind_addr);
    // Tanpa redirect: 301 pada POST tidak boleh diam-diam berubah menjadi GET.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let git: Arc<dyn GitProviderFactory> = Arc::new(HttpGitProviderFactory::new(http.clone()));
    let chat: Arc<dyn ChatService> =
        Arc::new(LlmRouter::new(db.clone(), cipher.clone(), http.clone()));
    let engine = Arc::new(Engine {
        db: db.clone(),
        cipher: cipher.clone(),
        git: git.clone(),
        chat: chat.clone(),
    });

    let shutdown = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".to_string());
    for index in 0..config.workers {
        tasks.spawn(run_worker(
            format!("worker-{host}-{index}"),
            queue.clone(),
            engine.clone(),
            JOB_TIMEOUT,
            shutdown.clone(),
        ));
    }
    tasks.spawn(run_scheduler(queue.clone(), shutdown.clone()));
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutting down: no new jobs, waiting for running jobs");
        signal_token.cancel();
    });

    let state = AppState {
        db,
        cipher,
        config: Arc::new(config),
        git,
        http,
        queue,
        chat,
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;

    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!("jobs still running after 30 s; exiting anyway");
    }
    Ok(())
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
