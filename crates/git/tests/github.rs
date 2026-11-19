use adus_git::{
    FileStatus, GitError, GitProvider, GithubProvider, InlineComment, PullRequest, RepoRef,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let p = format!(
        "{}/../../tests/fixtures/github/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(p).unwrap()
}

fn json_body(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

fn provider(server: &MockServer) -> GithubProvider {
    GithubProvider::new(reqwest::Client::new(), server.uri(), "ghp_secret".into())
}

fn repo() -> RepoRef {
    RepoRef {
        external_id: "1".into(),
        owner: "octocat".into(),
        project: None,
        name: "alpha".into(),
    }
}

fn pr() -> PullRequest {
    PullRequest {
        number: 7,
        title: "t".into(),
        description: String::new(),
        head_sha: "headsha".into(),
        base_sha: "mergebase".into(),
        is_draft: false,
    }
}

#[tokio::test]
async fn verify_returns_login() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer ghp_secret"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header("user-agent", "adus"))
        .respond_with(json_body("user.json"))
        .mount(&server)
        .await;

    let owner = provider(&server).verify().await.unwrap();
    assert_eq!(owner.id, "4242");
    assert_eq!(owner.login, "octocat");
    assert_eq!(owner.email.as_deref(), Some("octo@example.com"));
}

#[tokio::test]
async fn verify_401_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = provider(&server).verify().await.unwrap_err();
    assert_eq!(err, GitError::Unauthorized);
}

#[tokio::test]
async fn rate_limit_403_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
        .mount(&server)
        .await;

    let err = provider(&server).verify().await.unwrap_err();
    assert_eq!(err, GitError::RateLimited);
}

#[tokio::test]
async fn plain_403_is_http_error_without_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let err = provider(&server).verify().await.unwrap_err();
    assert!(matches!(&err, GitError::Http(m) if m.contains("403")));
    assert!(!err.to_string().contains("ghp_secret"));
}

#[tokio::test]
async fn list_repos_follows_link_pagination() {
    let server = MockServer::start().await;
    let next = format!("<{}/user/repos?page=2>; rel=\"next\"", server.uri());
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("page", "2"))
        .respond_with(json_body("repos_page2.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("per_page", "100"))
        .and(query_param(
            "affiliation",
            "owner,collaborator,organization_member",
        ))
        .respond_with(json_body("repos_page1.json").insert_header("link", next.as_str()))
        .mount(&server)
        .await;

    let repos = provider(&server).list_repos().await.unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].r#ref.external_id, "1");
    assert_eq!(repos[0].r#ref.owner, "octocat");
    assert_eq!(repos[0].r#ref.name, "alpha");
    assert_eq!(repos[0].r#ref.project, None);
    assert_eq!(repos[0].default_branch.as_deref(), Some("main"));
    assert_eq!(repos[1].r#ref.owner, "acme");
    assert_eq!(repos[1].default_branch, None);
}

#[tokio::test]
async fn ensure_webhooks_recreates_missing_hook() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/hooks/55"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/alpha/hooks"))
        .and(body_json(json!({
            "name": "web",
            "active": true,
            "events": ["pull_request", "issue_comment", "pull_request_review_comment"],
            "config": { "url": "https://adus.example/hook", "content_type": "json", "secret": "s3" },
        })))
        .respond_with(json_body("hook_created.json"))
        .expect(1)
        .mount(&server)
        .await;

    let ids = provider(&server)
        .ensure_webhooks(&repo(), "https://adus.example/hook", "s3", &["55".into()])
        .await
        .unwrap();
    assert_eq!(ids, vec!["999".to_string()]);
}

#[tokio::test]
async fn ensure_webhooks_keeps_existing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/hooks/55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 55 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/alpha/hooks"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let ids = provider(&server)
        .ensure_webhooks(&repo(), "https://adus.example/hook", "s3", &["55".into()])
        .await
        .unwrap();
    assert_eq!(ids, vec!["55".to_string()]);
}

#[tokio::test]
async fn delete_webhooks_treats_404_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/repos/octocat/alpha/hooks/1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/octocat/alpha/hooks/2"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .delete_webhooks(&repo(), &["1".into(), "2".into()])
        .await
        .unwrap();
}

#[tokio::test]
async fn get_pr_uses_merge_base_from_compare() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/pulls/7"))
        .respond_with(json_body("pull.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/compare/basesha...headsha"))
        .respond_with(json_body("compare.json"))
        .mount(&server)
        .await;

    let got = provider(&server).get_pr(&repo(), 7).await.unwrap();
    assert_eq!(got.number, 7);
    assert_eq!(got.title, "Add feature");
    assert_eq!(got.description, "");
    assert_eq!(got.head_sha, "headsha");
    assert_eq!(got.base_sha, "mergebase");
    assert!(got.is_draft);
}

#[tokio::test]
async fn pr_diff_paginates_and_marks_binary_patch_none() {
    let server = MockServer::start().await;
    let next = format!(
        "<{}/repos/octocat/alpha/pulls/7/files?page=2>; rel=\"next\", <{}/x?page=9>; rel=\"last\"",
        server.uri(),
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/pulls/7/files"))
        .and(query_param("page", "2"))
        .respond_with(json_body("pr_files_page2.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/pulls/7/files"))
        .and(query_param("per_page", "100"))
        .respond_with(json_body("pr_files_page1.json").insert_header("link", next.as_str()))
        .mount(&server)
        .await;

    let files = provider(&server).pr_diff(&repo(), &pr()).await.unwrap();
    assert_eq!(files.len(), 4);
    assert_eq!(files[0].path, "src/a.rs");
    assert_eq!(files[0].status, FileStatus::Modified);
    assert!(files[0].patch.is_some());
    assert_eq!(files[1].path, "logo.png");
    assert_eq!(files[1].status, FileStatus::Added);
    assert_eq!(files[1].patch, None);
    assert_eq!(files[2].path, "src/new_name.rs");
    assert_eq!(files[2].old_path.as_deref(), Some("src/old_name.rs"));
    assert_eq!(files[2].status, FileStatus::Renamed);
    assert_eq!(files[3].status, FileStatus::Deleted);
}

#[tokio::test]
async fn diff_between_reads_compare_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/compare/aaa...bbb"))
        .respond_with(json_body("compare.json"))
        .mount(&server)
        .await;

    let files = provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "src/new.rs");
    assert_eq!(files[0].status, FileStatus::Added);
}

#[tokio::test]
async fn post_review_sends_right_side_lines() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/alpha/pulls/7/reviews"))
        .and(body_json(json!({
            "commit_id": "headsha",
            "event": "COMMENT",
            "body": "ringkasan",
            "comments": [
                { "path": "src/a.rs", "line": 12, "side": "RIGHT", "body": "perbaiki" },
                { "path": "src/b.rs", "line": 3, "side": "RIGHT", "body": "oke?" },
            ],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    let comments = vec![
        InlineComment {
            path: "src/a.rs".into(),
            line: 12,
            body: "perbaiki".into(),
        },
        InlineComment {
            path: "src/b.rs".into(),
            line: 3,
            body: "oke?".into(),
        },
    ];
    let n = provider(&server)
        .post_review(&repo(), &pr(), &comments, "ringkasan")
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn post_review_without_comments_still_posts_summary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/alpha/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    let n = provider(&server)
        .post_review(&repo(), &pr(), &[], "ringkasan")
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn post_comment_posts_to_issue_comments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/alpha/issues/7/comments"))
        .and(body_json(json!({ "body": "halo" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(Value::Null))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .post_comment(&repo(), 7, "halo")
        .await
        .unwrap();
}

#[tokio::test]
async fn not_found_and_decode_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/alpha/pulls/404"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let p = provider(&server);
    assert_eq!(
        p.get_pr(&repo(), 404).await.unwrap_err(),
        GitError::NotFound
    );
    assert!(matches!(p.verify().await.unwrap_err(), GitError::Decode(_)));
}
