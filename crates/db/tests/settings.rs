use adus_core::default_ignore::DEFAULT_IGNORE_PATHS;
use adus_core::types::{ProviderKind, ReviewMode};
use adus_db::{
    ai,
    ignore::effective_ignore_paths,
    settings::{self, Settings},
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn settings_defaults_and_round_trip(pool: PgPool) {
    let current = settings::get(&pool).await.unwrap();
    assert_eq!(current.default_provider_id, None);
    assert_eq!(current.max_files_per_review, 30);
    assert_eq!(current.max_patch_lines_per_file, 1500);
    assert_eq!(current.small_pr_max_lines, 100);
    assert_eq!(current.max_review_input_tokens, 2_000_000);
    assert_eq!(current.review_mode, ReviewMode::Generalist);
    assert_eq!(current.ignore_paths, None);
    assert!(current.ignored_title_keywords.is_empty());
    assert_eq!(effective_ignore_paths(&current), DEFAULT_IGNORE_PATHS);

    let updated = Settings {
        default_provider_id: None,
        max_files_per_review: 10,
        max_patch_lines_per_file: 500,
        ignore_paths: Some(vec!["dist/**".into(), "*.min.js".into()]),
        ignored_title_keywords: vec!["wip".into()],
        review_mode: ReviewMode::Specialists,
        small_pr_max_lines: 120,
        max_review_input_tokens: 0,
    };
    let saved = settings::update(&pool, &updated).await.unwrap();
    assert_eq!(saved, updated);
    assert_eq!(settings::get(&pool).await.unwrap(), saved);
    assert_eq!(effective_ignore_paths(&saved), ["dist/**", "*.min.js"]);

    let mut reset = saved;
    reset.ignore_paths = None;
    let reset = settings::update(&pool, &reset).await.unwrap();
    assert_eq!(effective_ignore_paths(&reset), DEFAULT_IGNORE_PATHS);
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleting_default_provider_clears_default_provider_id(pool: PgPool) {
    let provider = ai::insert_provider(
        &pool,
        ProviderKind::Gemini,
        "g",
        None,
        "gemini-x",
        Some("gemini-fast"),
    )
    .await
    .unwrap();
    let mut updated = settings::get(&pool).await.unwrap();
    updated.default_provider_id = Some(provider.id);
    settings::update(&pool, &updated).await.unwrap();

    ai::delete_provider(&pool, provider.id).await.unwrap();
    assert_eq!(
        settings::get(&pool).await.unwrap().default_provider_id,
        None
    );
}
