use adus_core::types::{ConnectionKind, Severity};
use adus_db::{
    connections, memories,
    memories::{MemorySource, NewMemory},
    repositories::{self, NewRepository},
    rules::{self, KnowledgeScope, NewRule, Rule},
};
use sqlx::PgPool;
use uuid::Uuid;

fn rule_with_glob(glob: Option<&str>) -> Rule {
    let now = chrono::Utc::now();
    Rule {
        id: Uuid::new_v4(),
        scope: KnowledgeScope::Global,
        repository_id: None,
        title: "t".into(),
        body: "b".into(),
        path_glob: glob.map(str::to_string),
        severity: Severity::Warning,
        enabled: true,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn glob_matches_azure_style_path() {
    let rule = rule_with_glob(Some("src/**"));
    assert!(rules::rule_matches_path(&rule, "/src/a/b.rs"));
    assert!(rules::rule_matches_path(&rule, "src/a.rs"));
    assert!(!rules::rule_matches_path(&rule, "/docs/a.md"));

    // `*` melewati `/`.
    let rule = rule_with_glob(Some("*.rs"));
    assert!(rules::rule_matches_path(&rule, "src/a.rs"));
    assert!(!rules::rule_matches_path(&rule, "src/a.py"));
}

#[test]
fn null_glob_matches_all() {
    let rule = rule_with_glob(None);
    assert!(rules::rule_matches_path(&rule, "/anything/at/all.txt"));
}

#[test]
fn invalid_glob_matches_nothing() {
    assert!(!rules::validate_glob("src/[a"));
    assert!(rules::validate_glob("src/**/*.rs"));
    let rule = rule_with_glob(Some("src/[a"));
    assert!(!rules::rule_matches_path(&rule, "src/a.rs"));
}

async fn add_repo(pool: &PgPool, external_id: &str) -> Uuid {
    let conn = connections::insert(
        pool,
        ConnectionKind::Github,
        "c",
        "acme",
        "enc",
        "1234",
        "octocat",
    )
    .await
    .unwrap();
    let repo = repositories::insert(
        pool,
        NewRepository {
            connection_id: conn.id,
            external_id,
            owner: "acme",
            project: None,
            name: external_id,
            default_branch: None,
            webhook_token: "t",
        },
    )
    .await
    .unwrap()
    .unwrap();
    repo.id
}

async fn add_rule(pool: &PgPool, repo: Option<Uuid>, title: &str) -> Rule {
    rules::insert(
        pool,
        NewRule {
            scope: if repo.is_some() {
                KnowledgeScope::Repo
            } else {
                KnowledgeScope::Global
            },
            repository_id: repo,
            title,
            body: "body",
            path_glob: None,
            severity: Severity::Warning,
        },
    )
    .await
    .unwrap()
}

async fn add_memory(pool: &PgPool, repo: Option<Uuid>, content: &str) -> memories::Memory {
    memories::insert(
        pool,
        NewMemory {
            scope: if repo.is_some() {
                KnowledgeScope::Repo
            } else {
                KnowledgeScope::Global
            },
            repository_id: repo,
            content,
            source: MemorySource::Manual,
            source_url: None,
            created_by: None,
        },
    )
    .await
    .unwrap()
}

fn titles(rules: &[Rule]) -> Vec<&str> {
    rules.iter().map(|r| r.title.as_str()).collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn applicable_includes_global_and_own_repo_only(pool: PgPool) {
    let repo_a = add_repo(&pool, "a").await;
    let repo_b = add_repo(&pool, "b").await;
    add_rule(&pool, None, "global").await;
    add_rule(&pool, Some(repo_a), "of-a").await;
    add_rule(&pool, Some(repo_b), "of-b").await;
    let off = add_rule(&pool, None, "global-off").await;
    rules::update(
        &pool,
        off.id,
        rules::RulePatch {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let got = rules::applicable(&pool, repo_a).await.unwrap();
    assert_eq!(titles(&got), ["global", "of-a"]);

    // Memory mengikuti aturan yang sama.
    add_memory(&pool, None, "g").await;
    add_memory(&pool, Some(repo_a), "a").await;
    add_memory(&pool, Some(repo_b), "b").await;
    let got = memories::applicable(&pool, repo_a).await.unwrap();
    let contents: Vec<_> = got.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["g", "a"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn soft_deleted_repo_rules_and_memories_are_hidden(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    add_rule(&pool, None, "global").await;
    add_rule(&pool, Some(repo), "of-repo").await;
    add_memory(&pool, Some(repo), "of-repo").await;
    repositories::soft_delete(&pool, repo).await.unwrap();

    assert_eq!(
        titles(&rules::applicable(&pool, repo).await.unwrap()),
        ["global"]
    );
    assert_eq!(titles(&rules::list_all(&pool).await.unwrap()), ["global"]);
    assert!(rules::list(&pool, Some(repo)).await.unwrap().is_empty());
    assert!(memories::applicable(&pool, repo).await.unwrap().is_empty());
    assert!(memories::list_all(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_filters_by_scope(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    add_rule(&pool, None, "global").await;
    add_rule(&pool, Some(repo), "of-repo").await;

    assert_eq!(titles(&rules::list(&pool, None).await.unwrap()), ["global"]);
    assert_eq!(
        titles(&rules::list(&pool, Some(repo)).await.unwrap()),
        ["of-repo"]
    );
    assert_eq!(rules::list_all(&pool).await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn check_constraint_rejects_scope_mismatch(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let bad = rules::insert(
        &pool,
        NewRule {
            scope: KnowledgeScope::Repo,
            repository_id: None,
            title: "t",
            body: "b",
            path_glob: None,
            severity: Severity::Info,
        },
    )
    .await;
    assert!(bad.is_err());
    let bad = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Global,
            repository_id: Some(repo),
            content: "c",
            source: MemorySource::Manual,
            source_url: None,
            created_by: None,
        },
    )
    .await;
    assert!(bad.is_err());
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_can_clear_glob_and_keeps_other_fields(pool: PgPool) {
    let rule = rules::insert(
        &pool,
        NewRule {
            scope: KnowledgeScope::Global,
            repository_id: None,
            title: "t",
            body: "b",
            path_glob: Some("src/**"),
            severity: Severity::Critical,
        },
    )
    .await
    .unwrap();

    let same = rules::update(&pool, rule.id, rules::RulePatch::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(same.path_glob.as_deref(), Some("src/**"));
    assert_eq!(same.severity, Severity::Critical);

    let cleared = rules::update(
        &pool,
        rule.id,
        rules::RulePatch {
            path_glob: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(cleared.path_glob, None);
    assert!(rules::delete(&pool, rule.id).await.unwrap());
    assert!(!rules::delete(&pool, rule.id).await.unwrap());
}
