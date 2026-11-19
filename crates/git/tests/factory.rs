use adus_git::{ConnectionKind, GitProviderFactory, HttpGitProviderFactory};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn create_points_providers_at_configured_bases() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer ghp_x"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "id": 1, "login": "octocat", "email": null })),
        )
        .expect(1)
        .mount(&github)
        .await;
    let azure = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authenticatedUser": { "id": "u1", "providerDisplayName": "Ana" }
        })))
        .expect(1)
        .mount(&azure)
        .await;

    let factory = HttpGitProviderFactory {
        http: reqwest::Client::new(),
        github_base: github.uri(),
        azure_base: azure.uri(),
    };
    let gh = factory.create(ConnectionKind::Github, "octocat", "ghp_x");
    assert_eq!(gh.verify().await.unwrap().login, "octocat");
    let az = factory.create(ConnectionKind::AzureDevops, "acme", "azpat");
    assert_eq!(az.verify().await.unwrap().login, "Ana");
}

#[test]
fn new_uses_public_defaults() {
    let factory = HttpGitProviderFactory::new(reqwest::Client::new());
    assert_eq!(factory.github_base, "https://api.github.com");
    assert_eq!(factory.azure_base, "https://dev.azure.com");
}
