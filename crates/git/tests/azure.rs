use adus_git::{
    AzureProvider, FileStatus, GitError, GitProvider, InlineComment, PullRequest, RepoRef,
};
use serde_json::{Value, json};
use wiremock::matchers::{
    body_json, body_partial_json, header, header_exists, method, path, query_param,
    query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT: &str = "pppp0000-0000-0000-0000-000000000001";
const REPO_ID: &str = "rrrr0000-0000-0000-0000-000000000001";
const REPO_BASE: &str = "/acme/pppp0000-0000-0000-0000-000000000001/_apis/git/repositories/rrrr0000-0000-0000-0000-000000000001";

fn fixture(name: &str) -> String {
    let p = format!(
        "{}/../../tests/fixtures/azure/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(p).unwrap()
}

fn json_body(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

fn provider(server: &MockServer) -> AzureProvider {
    AzureProvider::new(
        reqwest::Client::new(),
        server.uri(),
        "acme".into(),
        "azpat".into(),
    )
}

fn repo() -> RepoRef {
    RepoRef {
        external_id: REPO_ID.into(),
        owner: "acme".into(),
        project: Some(PROJECT.into()),
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
        source_ref: None,
    }
}

/// Mock isi file `file_path` pada commit `sha`.
async fn mock_item(server: &MockServer, file_path: &str, sha: &str, body: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/items")))
        .and(query_param("path", file_path))
        .and(query_param("versionDescriptor.version", sha))
        .and(query_param("versionDescriptor.versionType", "commit"))
        .and(query_param("$format", "octetStream"))
        .and(query_param("api-version", "7.1"))
        .respond_with(body)
        .mount(server)
        .await;
}

fn text(s: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_bytes(s.as_bytes().to_vec())
}

fn bytes(b: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_bytes(b.to_vec())
}

async fn list_repos_error(status: ResponseTemplate) -> GitError {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/git/repositories"))
        .respond_with(status)
        .mount(&server)
        .await;
    provider(&server).list_repos().await.unwrap_err()
}

#[tokio::test]
async fn verify_reads_connection_data_email() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .and(query_param_is_missing("api-version"))
        .respond_with(json_body("connection_data.json"))
        .mount(&server)
        .await;

    let owner = provider(&server).verify().await.unwrap();
    assert_eq!(owner.id, "aaaa1111-0000-0000-0000-000000000001");
    assert_eq!(owner.login, "Ana Dev");
    assert_eq!(owner.email.as_deref(), Some("ana@example.com"));
}

#[tokio::test]
async fn basic_auth_header_is_colon_pat() {
    let server = MockServer::start().await;
    // base64(":azpat")
    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .and(header("authorization", "Basic OmF6cGF0"))
        .respond_with(json_body("connection_data.json"))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server).verify().await.unwrap();
}

#[tokio::test]
async fn status_203_sign_in_page_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .respond_with(ResponseTemplate::new(203).set_body_string("<html>sign in</html>"))
        .mount(&server)
        .await;

    let err = provider(&server).verify().await.unwrap_err();
    assert_eq!(err, GitError::Unauthorized);
}

#[tokio::test]
async fn error_statuses_map_without_leaking_token() {
    assert_eq!(
        list_repos_error(ResponseTemplate::new(401)).await,
        GitError::Unauthorized
    );
    assert_eq!(
        list_repos_error(ResponseTemplate::new(429).insert_header("retry-after", "5")).await,
        GitError::RateLimited
    );
    assert_eq!(
        list_repos_error(ResponseTemplate::new(404)).await,
        GitError::NotFound
    );
    let err = list_repos_error(ResponseTemplate::new(500)).await;
    assert_eq!(err, GitError::Status(500));
    assert!(!err.to_string().contains("azpat"));
}

#[tokio::test]
async fn list_repos_reads_project_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/git/repositories"))
        .respond_with(json_body("repositories.json"))
        .mount(&server)
        .await;

    let repos = provider(&server).list_repos().await.unwrap();
    assert_eq!(repos[0].project_name.as_deref(), Some("Shop"));
    assert_eq!(repos[1].project_name.as_deref(), Some("Shop"));
    // `project` tetap GUID untuk URL API.
    assert_eq!(repos[0].r#ref.project.as_deref(), Some(PROJECT));
}

#[tokio::test]
async fn list_repos_uses_guids_and_strips_branch_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/git/repositories"))
        .and(query_param("api-version", "7.1"))
        .respond_with(json_body("repositories.json"))
        .mount(&server)
        .await;

    let repos = provider(&server).list_repos().await.unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].r#ref, repo());
    assert_eq!(repos[0].default_branch.as_deref(), Some("main"));
    assert_eq!(repos[1].default_branch, None);
}

#[tokio::test]
async fn get_pr_reads_is_draft() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/pullrequests/7")))
        .and(query_param("api-version", "7.1"))
        .respond_with(json_body("pr.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/pullRequests/7/iterations")))
        .respond_with(json_body("iterations.json"))
        .mount(&server)
        .await;

    let got = provider(&server).get_pr(&repo(), 7).await.unwrap();
    assert!(got.is_draft);
    assert_eq!(got.number, 7);
    assert_eq!(got.title, "Add feature");
    assert_eq!(got.description, "");
    assert_eq!(got.head_sha, "headsha");
    // commonRefCommit dari iterasi terakhir (id 2), bukan iterasi 1.
    assert_eq!(got.base_sha, "mergebase");
    assert_eq!(got.source_ref.as_deref(), Some("refs/heads/feature"));
}

#[tokio::test]
async fn ensure_webhooks_creates_three_subscriptions() {
    let server = MockServer::start().await;
    let events = [
        "git.pullrequest.created",
        "git.pullrequest.updated",
        "ms.vss-code.git-pullrequest-comment-event",
    ];
    for (n, event) in events.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/acme/_apis/hooks/subscriptions"))
            .and(query_param("api-version", "7.1"))
            .and(body_json(json!({
                "publisherId": "tfs",
                "eventType": event,
                "consumerId": "webHooks",
                "consumerActionId": "httpRequest",
                "publisherInputs": { "projectId": PROJECT, "repository": REPO_ID },
                "consumerInputs": { "url": "https://adus.test/webhooks/azure/1" },
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": format!("sub-{n}"), "eventType": event })),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let ids = provider(&server)
        .ensure_webhooks(&repo(), "https://adus.test/webhooks/azure/1", "s", &[])
        .await
        .unwrap();
    assert_eq!(ids, vec!["sub-0", "sub-1", "sub-2"]);
}

fn healthy_sub(id: &str, event: &str, url: &str) -> Value {
    json!({
        "id": id,
        "eventType": event,
        "status": "enabled",
        "consumerInputs": { "url": url },
    })
}

#[tokio::test]
async fn ensure_webhooks_recreates_only_missing() {
    let server = MockServer::start().await;
    let sub = |id: &str, event: &str| healthy_sub(id, event, "https://adus.test/h");
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/keep-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sub("keep-1", "git.pullrequest.created")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/keep-2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sub("keep-2", "git.pullrequest.updated")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    // Hanya event komentar yang dibuat ulang.
    let comment_event = "ms.vss-code.git-pullrequest-comment-event";
    Mock::given(method("POST"))
        .and(path("/acme/_apis/hooks/subscriptions"))
        .and(body_partial_json(json!({ "eventType": comment_event })))
        .respond_with(ResponseTemplate::new(200).set_body_json(sub("new-3", comment_event)))
        .expect(1)
        .mount(&server)
        .await;

    let existing = ["keep-1".to_string(), "keep-2".into(), "gone".into()];
    let ids = provider(&server)
        .ensure_webhooks(&repo(), "https://adus.test/h", "s", &existing)
        .await
        .unwrap();
    assert_eq!(ids, vec!["keep-1", "keep-2", "new-3"]);
}

#[tokio::test]
async fn webhooks_exist_false_when_one_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(healthy_sub(
            "a",
            "git.pullrequest.created",
            "u",
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/b"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let p = provider(&server);
    assert!(
        p.webhooks_exist(&repo(), &["a".to_string()], "u")
            .await
            .unwrap()
    );
    assert!(
        !p.webhooks_exist(&repo(), &["a".to_string(), "b".into()], "u")
            .await
            .unwrap()
    );
    // URL publik berubah: subscription lama tidak lagi dianggap ada.
    assert!(
        !p.webhooks_exist(&repo(), &["a".to_string()], "https://new.example/h")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn webhooks_exist_false_when_subscription_disabled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/hooks/subscriptions/off"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "off",
            "eventType": "git.pullrequest.created",
            "status": "disabledByUser",
            "consumerInputs": { "url": "u" },
        })))
        .mount(&server)
        .await;

    assert!(
        !provider(&server)
            .webhooks_exist(&repo(), &["off".to_string()], "u")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn delete_webhooks_treats_404_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/acme/_apis/hooks/subscriptions/a"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/acme/_apis/hooks/subscriptions/b"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .delete_webhooks(&repo(), &["a".to_string(), "b".into()])
        .await
        .unwrap();
}

/// Iterasi (id terakhir = 2) dan dua halaman perubahan (2 entri + 4 entri).
async fn mock_pr_iterations_and_changes(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/pullRequests/7/iterations")))
        .respond_with(json_body("iterations.json"))
        .mount(server)
        .await;
    let changes = format!("{REPO_BASE}/pullRequests/7/iterations/2/changes");
    Mock::given(method("GET"))
        .and(path(&changes))
        .and(query_param("$compareTo", "0"))
        .and(query_param("$skip", "0"))
        .respond_with(json_body("changes_page1.json"))
        .expect(1)
        .mount(server)
        .await;
    // Halaman 1 punya 2 entri, jadi halaman berikutnya `$skip=2`.
    Mock::given(method("GET"))
        .and(path(&changes))
        .and(query_param("$skip", "2"))
        .respond_with(json_body("changes_page2.json"))
        .expect(1)
        .mount(server)
        .await;
}

const PNG_OLD: &[u8] = &[0x89, 0x50, 0xff, 0xfe];
const PNG_NEW: &[u8] = &[0x89, 0x50, 0xff, 0xfd];

#[tokio::test]
async fn pr_diff_builds_patch_from_contents_and_strips_leading_slash() {
    let server = MockServer::start().await;
    mock_pr_iterations_and_changes(&server).await;
    mock_item(&server, "/src/a.rs", "mergebase", text("a\nb\n")).await;
    mock_item(&server, "/src/a.rs", "headsha", text("a\nB\n")).await;
    // File baru: sisi base tidak diambil. File terhapus: sisi head tidak diambil.
    mock_item(&server, "/src/new.rs", "headsha", text("x\n")).await;
    mock_item(&server, "/src/gone.rs", "mergebase", text("y\n")).await;
    // Rename: isi lama diambil dari path lama.
    mock_item(&server, "/src/orig.rs", "mergebase", text("1\n2\n")).await;
    mock_item(&server, "/src/moved.rs", "headsha", text("1\n3\n")).await;
    mock_item(&server, "/img/logo.png", "mergebase", bytes(PNG_OLD)).await;
    mock_item(&server, "/img/logo.png", "headsha", bytes(PNG_NEW)).await;

    let files = provider(&server).pr_diff(&repo(), &pr()).await.unwrap();
    // Folder `/src` dilewati; path tanpa `/` di depan.
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "src/a.rs",
            "src/new.rs",
            "src/gone.rs",
            "src/moved.rs",
            "img/logo.png"
        ]
    );

    assert_eq!(files[0].status, FileStatus::Modified);
    assert_eq!(
        files[0].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
    );
    assert_eq!(files[1].status, FileStatus::Added);
    assert_eq!(files[1].patch.as_deref(), Some("@@ -0,0 +1 @@\n+x\n"));
    assert_eq!(files[2].status, FileStatus::Deleted);
    assert_eq!(files[2].patch.as_deref(), Some("@@ -1 +0,0 @@\n-y\n"));
    assert_eq!(files[3].status, FileStatus::Renamed);
    assert_eq!(files[3].old_path.as_deref(), Some("src/orig.rs"));
    assert_eq!(
        files[3].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n 1\n-2\n+3\n")
    );
}

#[tokio::test]
async fn binary_content_gives_none_patch() {
    let server = MockServer::start().await;
    mock_pr_iterations_and_changes(&server).await;
    for (p, sha) in [
        ("/src/a.rs", "mergebase"),
        ("/src/a.rs", "headsha"),
        ("/src/new.rs", "headsha"),
        ("/src/gone.rs", "mergebase"),
        ("/src/orig.rs", "mergebase"),
        ("/src/moved.rs", "headsha"),
    ] {
        mock_item(&server, p, sha, text("t\n")).await;
    }
    mock_item(&server, "/img/logo.png", "mergebase", bytes(PNG_OLD)).await;
    mock_item(&server, "/img/logo.png", "headsha", bytes(PNG_NEW)).await;

    let files = provider(&server).pr_diff(&repo(), &pr()).await.unwrap();
    let png = files.iter().find(|f| f.path == "img/logo.png").unwrap();
    assert_eq!(png.patch, None);
    // Teks lain tetap dapat patch: a.rs sama (kosong), new.rs +t, gone.rs -t, moved.rs sama.
    let patches: Vec<_> = files.iter().map(|f| f.patch.as_deref()).collect();
    assert_eq!(
        patches,
        [
            Some(""),
            Some("@@ -0,0 +1 @@\n+t\n"),
            Some("@@ -1 +0,0 @@\n-t\n"),
            Some(""),
            None
        ]
    );
}

#[tokio::test]
async fn diff_between_reads_commit_diffs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/diffs/commits")))
        .and(query_param("baseVersion", "aaa"))
        .and(query_param("baseVersionType", "commit"))
        .and(query_param("targetVersion", "bbb"))
        .and(query_param("targetVersionType", "commit"))
        .and(query_param("api-version", "7.1"))
        .respond_with(json_body("diffs.json"))
        .expect(1)
        .mount(&server)
        .await;
    mock_item(&server, "/src/a.rs", "aaa", text("a\nb\n")).await;
    mock_item(&server, "/src/a.rs", "bbb", text("a\nB\n")).await;

    let files = provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "src/a.rs");
    assert_eq!(
        files[0].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
    );
}

#[tokio::test]
async fn diff_between_pages_while_not_all_changes_included() {
    let server = MockServer::start().await;
    let diffs = format!("{REPO_BASE}/diffs/commits");
    let entry = |p: &str| json!({ "changeType": "add", "item": { "path": p } });
    Mock::given(method("GET"))
        .and(path(&diffs))
        .and(query_param("$skip", "0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "allChangesIncluded": false, "changes": [entry("/one.txt")] }),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(&diffs))
        .and(query_param("$skip", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "allChangesIncluded": true, "changes": [entry("/two.txt")] }),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    mock_item(&server, "/one.txt", "bbb", text("1\n")).await;
    mock_item(&server, "/two.txt", "bbb", text("2\n")).await;

    let files = provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap();
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["one.txt", "two.txt"]);
}

async fn captured_threads(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/threads"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn post_review_thread_context_has_leading_slash() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{REPO_BASE}/pullRequests/7/threads")))
        .and(query_param("api-version", "7.1"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 1 })))
        .expect(2)
        .mount(&server)
        .await;

    let comments = [InlineComment {
        path: "src/a.rs".into(),
        line: 12,
        body: "careful".into(),
    }];
    let n = provider(&server)
        .post_review(&repo(), &pr(), &comments, "summary")
        .await
        .unwrap();
    assert_eq!(n, 1);

    let threads = captured_threads(&server).await;
    assert_eq!(
        threads[0],
        json!({
            "comments": [{ "content": "careful", "commentType": 1 }],
            "status": 1,
            "threadContext": {
                "filePath": "/src/a.rs",
                "rightFileStart": { "line": 12, "offset": 1 },
                "rightFileEnd": { "line": 12, "offset": 1 },
            },
        })
    );
    // Ringkasan = thread tanpa threadContext.
    assert_eq!(
        threads[1],
        json!({ "comments": [{ "content": "summary", "commentType": 1 }], "status": 1 })
    );
}

#[tokio::test]
async fn post_comment_posts_thread_without_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{REPO_BASE}/pullRequests/7/threads")))
        .and(body_json(
            json!({ "comments": [{ "content": "hi", "commentType": 1 }], "status": 1 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .post_comment(&repo(), 7, "hi")
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_project_is_http_error() {
    let server = MockServer::start().await;
    let mut r = repo();
    r.project = None;
    let p = provider(&server);
    let missing = GitError::Http("missing project".into());
    assert_eq!(p.get_pr(&r, 7).await.unwrap_err(), missing);
    assert_eq!(
        p.ensure_webhooks(&r, "u", "s", &[]).await.unwrap_err(),
        missing
    );
    assert_eq!(p.post_comment(&r, 7, "x").await.unwrap_err(), missing);
}

#[tokio::test]
async fn changes_pagination_stops_at_page_cap() {
    let server = MockServer::start().await;
    // Server selalu bilang masih ada halaman berikutnya.
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/diffs/commits")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allChangesIncluded": false,
            "changes": [{ "changeType": "add", "item": { "path": "/f.txt", "isFolder": true } }],
        })))
        .expect(30)
        .mount(&server)
        .await;

    let files = provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap();
    // Semua entri folder, jadi tidak ada file; yang diuji: berhenti di 30 request.
    assert!(files.is_empty());
}

#[tokio::test]
async fn ensure_webhooks_replaces_disabled_and_wrong_url_subscriptions() {
    let server = MockServer::start().await;
    let url = "https://adus.test/h";
    let created = "git.pullrequest.created";
    let updated = "git.pullrequest.updated";
    let comment = "ms.vss-code.git-pullrequest-comment-event";
    let get = |id: &str, body: Value| {
        Mock::given(method("GET"))
            .and(path(format!("/acme/_apis/hooks/subscriptions/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    };
    let mut disabled = healthy_sub("dead", created, url);
    disabled["status"] = json!("disabledBySystem");
    get("dead", disabled).mount(&server).await;
    get("stale", healthy_sub("stale", updated, "https://old.test/h"))
        .mount(&server)
        .await;
    get("ok", healthy_sub("ok", comment, url))
        .mount(&server)
        .await;
    // Yang nonaktif/salah URL dihapus (404 saat hapus diabaikan), yang sehat tidak disentuh.
    for (id, status) in [("dead", 204), ("stale", 404)] {
        Mock::given(method("DELETE"))
            .and(path(format!("/acme/_apis/hooks/subscriptions/{id}")))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("DELETE"))
        .and(path("/acme/_apis/hooks/subscriptions/ok"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    for (id, event) in [("new-1", created), ("new-2", updated)] {
        Mock::given(method("POST"))
            .and(path("/acme/_apis/hooks/subscriptions"))
            .and(body_partial_json(json!({ "eventType": event })))
            .respond_with(ResponseTemplate::new(200).set_body_json(healthy_sub(id, event, url)))
            .expect(1)
            .mount(&server)
            .await;
    }

    let existing = ["dead".to_string(), "stale".into(), "ok".into()];
    let ids = provider(&server)
        .ensure_webhooks(&repo(), url, "s", &existing)
        .await
        .unwrap();
    assert_eq!(ids, vec!["ok", "new-1", "new-2"]);
}

/// Satu file `Modified` di `diffs/commits` aaa..bbb; `base`/`head` = respons item per sisi.
async fn modified_file_error(base: ResponseTemplate, head: ResponseTemplate) -> GitError {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/diffs/commits")))
        .respond_with(json_body("diffs.json"))
        .mount(&server)
        .await;
    mock_item(&server, "/src/a.rs", "aaa", base).await;
    mock_item(&server, "/src/a.rs", "bbb", head).await;
    provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap_err()
}

#[tokio::test]
async fn content_404_on_a_fetched_side_is_not_found() {
    let not_found = || ResponseTemplate::new(404);
    // 404 di base, di head, dan di keduanya: error, bukan patch kosong / add / delete diam-diam.
    assert_eq!(
        modified_file_error(not_found(), text("a\n")).await,
        GitError::NotFound
    );
    assert_eq!(
        modified_file_error(text("a\n"), not_found()).await,
        GitError::NotFound
    );
    assert_eq!(
        modified_file_error(not_found(), not_found()).await,
        GitError::NotFound
    );
}

#[tokio::test]
async fn pr_diff_uses_iteration_matching_head_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/pullRequests/7/iterations")))
        .respond_with(json_body("iterations.json"))
        .mount(&server)
        .await;
    // Hanya iterasi 1 yang dipakai; iterasi 2 (lebih baru) tidak boleh diminta.
    Mock::given(method("GET"))
        .and(path(format!(
            "{REPO_BASE}/pullRequests/7/iterations/1/changes"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changeEntries": [{ "changeType": "edit", "item": { "path": "/src/a.rs" } }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{REPO_BASE}/pullRequests/7/iterations/2/changes"
        )))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    // Isi file di commit milik iterasi 1, bukan `pr.base_sha`.
    mock_item(&server, "/src/a.rs", "oldbase", text("a\nb\n")).await;
    mock_item(&server, "/src/a.rs", "oldhead", text("a\nB\n")).await;

    let mut stale = pr();
    stale.head_sha = "oldhead".into();
    stale.base_sha = "ignored".into();
    let files = provider(&server).pr_diff(&repo(), &stale).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
    );
}

#[tokio::test]
async fn pr_diff_falls_back_to_latest_iteration_shas() {
    let server = MockServer::start().await;
    mock_pr_iterations_and_changes(&server).await;
    for (p, sha, body) in [
        ("/src/a.rs", "mergebase", "a\nb\n"),
        ("/src/a.rs", "headsha", "a\nB\n"),
        ("/src/new.rs", "headsha", "x\n"),
        ("/src/gone.rs", "mergebase", "y\n"),
        ("/src/orig.rs", "mergebase", "1\n"),
        ("/src/moved.rs", "headsha", "1\n"),
        ("/img/logo.png", "mergebase", "p\n"),
        ("/img/logo.png", "headsha", "p\n"),
    ] {
        mock_item(&server, p, sha, text(body)).await;
    }

    // Tidak ada iterasi dengan head "unknown": pakai iterasi terbaru dan shas miliknya.
    let mut unknown = pr();
    unknown.head_sha = "unknown".into();
    unknown.base_sha = "ignored".into();
    let files = provider(&server).pr_diff(&repo(), &unknown).await.unwrap();
    assert_eq!(
        files[0].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
    );
}

#[tokio::test]
async fn rename_entries_and_submodules() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_BASE}/diffs/commits")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allChangesIncluded": true,
            "changes": [
                // Entri sisi-lama rename: dilewati.
                { "changeType": "delete, sourceRename", "item": { "path": "/old.txt" } },
                // "add, rename" = Renamed (bukan Added), isi lama dari path lama.
                { "changeType": "add, rename", "item": { "path": "/new.txt" }, "originalPath": "/old.txt" },
                // Submodule: dilewati.
                { "changeType": "edit", "item": { "path": "/vendor/lib", "gitObjectType": "commit" } },
            ],
        })))
        .mount(&server)
        .await;
    mock_item(&server, "/old.txt", "aaa", text("1\n2\n")).await;
    mock_item(&server, "/new.txt", "bbb", text("1\n3\n")).await;

    let files = provider(&server)
        .diff_between(&repo(), "aaa", "bbb")
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "new.txt");
    assert_eq!(files[0].status, FileStatus::Renamed);
    assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
    assert_eq!(
        files[0].patch.as_deref(),
        Some("@@ -1,2 +1,2 @@\n 1\n-2\n+3\n")
    );
}

#[test]
fn azure_clone_spec_uses_project_guid_and_basic_auth() {
    use base64::Engine;
    let p = AzureProvider::new(
        reqwest::Client::new(),
        "https://dev.azure.com/".into(),
        "acme".into(),
        "azpat".into(),
    );
    let mut r = repo();
    r.name = "my repo".into();
    let mut pull = pr();
    pull.source_ref = Some("refs/heads/feature".into());

    let spec = p.clone_spec(&r, &pull).unwrap();
    assert_eq!(
        spec.url,
        format!("https://dev.azure.com/acme/{PROJECT}/_git/my%20repo")
    );
    assert_eq!(spec.refspec, "headsha");
    assert_eq!(spec.fallback_refspec.as_deref(), Some("refs/heads/feature"));
    assert_eq!(spec.expected_sha, "headsha");
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(":azpat")
    );
    assert_eq!(spec.auth_header, expected);
    assert!(!format!("{spec:?}").contains(&spec.auth_header));

    let mut no_project = repo();
    no_project.project = None;
    assert!(p.clone_spec(&no_project, &pull).is_none());
    let mut empty_name = repo();
    empty_name.name = String::new();
    assert!(p.clone_spec(&empty_name, &pull).is_none());
}
