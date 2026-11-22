use adus_core::default_ignore::DEFAULT_IGNORE_PATHS;
use adus_core::types::ReviewMode;
use adus_db::{ignore::effective_ignore_paths, settings};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn settings_ignore_paths_null_means_defaults(pool: PgPool) {
    let current = settings::get(&pool).await.unwrap();
    assert_eq!(current.ignore_paths, None);
    assert!(current.ignored_title_keywords.is_empty());
    assert_eq!(effective_ignore_paths(&current), DEFAULT_IGNORE_PATHS);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_ignore_paths_round_trip_and_reset(pool: PgPool) {
    let mut new = settings::get(&pool).await.unwrap();
    new.ignore_paths = Some(vec!["dist/**".into(), "*.min.js".into()]);
    new.ignored_title_keywords = vec!["wip".into()];
    let saved = settings::update(&pool, &new).await.unwrap();
    assert_eq!(saved, new);
    assert_eq!(effective_ignore_paths(&saved), ["dist/**", "*.min.js"]);

    new.ignore_paths = None;
    let reset = settings::update(&pool, &new).await.unwrap();
    assert_eq!(effective_ignore_paths(&reset), DEFAULT_IGNORE_PATHS);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_review_mode_defaults_to_generalist_and_round_trips(pool: PgPool) {
    let mut current = settings::get(&pool).await.unwrap();
    assert_eq!(current.review_mode, ReviewMode::Generalist);

    current.review_mode = ReviewMode::Specialists;
    let saved = settings::update(&pool, &current).await.unwrap();
    assert_eq!(saved.review_mode, ReviewMode::Specialists);
    assert_eq!(settings::get(&pool).await.unwrap(), saved);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_defaults_are_30_files_1500_lines_100_small_pr_2m_tokens(pool: PgPool) {
    let current = settings::get(&pool).await.unwrap();
    assert_eq!(current.max_files_per_review, 30);
    assert_eq!(current.max_patch_lines_per_file, 1500);
    assert_eq!(current.small_pr_max_lines, 100);
    assert_eq!(current.max_review_input_tokens, 2_000_000);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_max_review_input_tokens_round_trips(pool: PgPool) {
    let mut current = settings::get(&pool).await.unwrap();
    current.max_review_input_tokens = 0;
    let saved = settings::update(&pool, &current).await.unwrap();
    assert_eq!(saved.max_review_input_tokens, 0);
    assert_eq!(settings::get(&pool).await.unwrap(), saved);
}
