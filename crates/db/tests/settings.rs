use adus_core::default_ignore::DEFAULT_IGNORE_PATHS;
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
