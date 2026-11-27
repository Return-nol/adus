//! Provider GitHub (REST v3) dengan PAT.

use async_trait::async_trait;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use adus_core::types::is_adus_comment;

use crate::{
    CloneSpec, DECODE_DETAIL_CHARS, FileDiff, FileStatus, GitError, GitProvider, InlineComment,
    PatOwner, PostedComment, PrThread, PrThreadComment, PullRequest, RemoteRepo, RepoRef,
    ThreadRef, ThreadStatus, basic_auth_header, decode, git_url, normalize_path,
};

/// Batas halaman saat mengikuti header `Link` (30 x 100 = 3000 item).
const MAX_PAGES: usize = 30;

pub struct GithubProvider {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl GithubProvider {
    /// `http` dibuat pemanggil (timeout diatur di sana). `base_url` contoh `https://api.github.com`.
    pub fn new(http: reqwest::Client, base_url: String, token: String) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn repo_url(&self, repo: &RepoRef, rest: &str) -> String {
        format!(
            "{}/repos/{}/{}{}",
            self.base_url, repo.owner, repo.name, rest
        )
    }

    fn get(&self, url: &str) -> RequestBuilder {
        self.with_headers(self.http.get(url))
    }

    fn post(&self, url: &str) -> RequestBuilder {
        self.with_headers(self.http.post(url))
    }

    fn with_headers(&self, req: RequestBuilder) -> RequestBuilder {
        req.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "adus")
    }

    /// Kirim request lalu petakan status gagal ke `GitError`. Token tidak pernah masuk pesan error.
    async fn send(&self, req: RequestBuilder) -> Result<Response, GitError> {
        let resp = req
            .send()
            .await
            .map_err(|e| GitError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let rate_limit_exhausted = resp
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0");
        // Secondary rate limit GitHub: 403 dengan `Retry-After`.
        let retry_after = resp.headers().contains_key("retry-after");
        Err(match status {
            StatusCode::UNAUTHORIZED => GitError::Unauthorized,
            StatusCode::TOO_MANY_REQUESTS => GitError::RateLimited,
            StatusCode::FORBIDDEN if rate_limit_exhausted || retry_after => GitError::RateLimited,
            StatusCode::NOT_FOUND => GitError::NotFound,
            other => GitError::Status(other.as_u16()),
        })
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, GitError> {
        decode(self.send(self.get(url)).await?).await
    }

    /// Ambil semua halaman (mengikuti `Link: rel="next"`, maks `MAX_PAGES`). Tiap halaman di-decode ke `P`.
    /// URL `next` harus di bawah `base_url`; selain itu berhenti dengan error (PAT tidak dikirim ke host lain).
    async fn get_pages<P: DeserializeOwned>(&self, first_url: String) -> Result<Vec<P>, GitError> {
        let mut pages = Vec::new();
        let mut url = first_url;
        loop {
            let resp = self.send(self.get(&url)).await?;
            let next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(next_link);
            pages.push(decode(resp).await?);
            let Some(next) = next else { break };
            if !next.starts_with(&format!("{}/", self.base_url)) {
                return Err(GitError::Http("unexpected pagination host".into()));
            }
            if pages.len() == MAX_PAGES {
                tracing::warn!(max_pages = MAX_PAGES, "github pagination truncated");
                break;
            }
            url = next;
        }
        Ok(pages)
    }

    /// `https://api.github.com/graphql`; GHES `{origin}/api/v3` → `{origin}/api/graphql`.
    fn graphql_url(&self) -> String {
        match self.base_url.strip_suffix("/api/v3") {
            Some(origin) => format!("{origin}/api/graphql"),
            None => format!("{}/graphql", self.base_url),
        }
    }

    /// One GraphQL call. GraphQL answers errors with HTTP 200 and an `errors` list:
    /// `NOT_FOUND` → `NotFound`, `RATE_LIMITED` → `RateLimited`, anything else → `Decode`.
    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T, GitError> {
        let body = json!({ "query": query, "variables": variables });
        let req = self.post(&self.graphql_url()).json(&body);
        let resp: GqlResponse = decode(self.send(req).await?).await?;
        if let Some(err) = resp.errors.first() {
            return Err(match err.kind.as_deref() {
                Some("NOT_FOUND") => GitError::NotFound,
                Some("RATE_LIMITED") => GitError::RateLimited,
                _ => GitError::Decode(
                    format!("graphql: {}", err.message)
                        .chars()
                        .take(DECODE_DETAIL_CHARS)
                        .collect(),
                ),
            });
        }
        serde_json::from_value(resp.data.unwrap_or(Value::Null)).map_err(|e| {
            GitError::Decode(e.to_string().chars().take(DECODE_DETAIL_CHARS).collect())
        })
    }

    /// The review thread whose first comment is `root`: `(node id, isResolved)`.
    async fn review_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        root: i64,
    ) -> Result<(String, bool), GitError> {
        let root = root.to_string();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let variables = json!({
                "owner": repo.owner,
                "name": repo.name,
                "number": pr_number,
                "after": after,
            });
            let data: GqlRepoData = self.graphql(REVIEW_THREADS_QUERY, variables).await?;
            let threads = data
                .repository
                .and_then(|r| r.pull_request)
                .ok_or(GitError::NotFound)?
                .review_threads;
            let found = threads.nodes.into_iter().flatten().find(|t| {
                t.comments
                    .nodes
                    .iter()
                    .flatten()
                    .next()
                    .and_then(|c| c.full_database_id.as_deref())
                    == Some(root.as_str())
            });
            if let Some(t) = found {
                return Ok((t.id, t.is_resolved));
            }
            match threads.page_info.end_cursor {
                Some(cursor) if threads.page_info.has_next_page => after = Some(cursor),
                _ => return Err(GitError::NotFound),
            }
        }
        tracing::warn!(max_pages = MAX_PAGES, "github review threads truncated");
        Err(GitError::NotFound)
    }

    /// The comments of review `review_id`, matched to `comments` by `(path, line)` in order.
    /// Unmatched comments are left out.
    async fn posted_ids(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        review_id: i64,
        comments: &[InlineComment],
    ) -> Result<Vec<PostedComment>, GitError> {
        let url = self.repo_url(
            repo,
            &format!("/pulls/{pr_number}/reviews/{review_id}/comments?per_page=100"),
        );
        let pages: Vec<Vec<GhComment>> = self.get_pages(url).await?;
        let mut created: Vec<GhComment> = pages.into_iter().flatten().collect();
        created.sort_by_key(|c| c.id);
        let mut posted = Vec::new();
        for (index, c) in comments.iter().enumerate() {
            let path = normalize_path(&c.path);
            let hit = created.iter().position(|g| {
                g.line == Some(c.line)
                    && g.path.as_deref().is_some_and(|p| normalize_path(p) == path)
            });
            if let Some(i) = hit {
                posted.push(PostedComment {
                    index,
                    thread_id: None,
                    comment_id: created.remove(i).id.to_string(),
                });
            }
        }
        Ok(posted)
    }

    async fn files_of(&self, first_url: String) -> Result<Vec<FileDiff>, GitError> {
        let pages: Vec<Vec<GhFile>> = self.get_pages(first_url).await?;
        Ok(pages.into_iter().flatten().map(GhFile::into_diff).collect())
    }
}

/// URL `rel="next"` dari nilai header `Link`, kalau ada. URL bisa memuat koma literal
/// (`affiliation=a,b`), jadi di-parse lewat `<...>`, bukan dipecah di koma.
fn next_link(link: &str) -> Option<String> {
    let mut rest = link;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after.find('>')?;
        let url = &after[..end];
        let tail = &after[end + 1..];
        // Parameter berlaku sampai `<` berikutnya.
        let (params, remaining) = match tail.find('<') {
            Some(i) => tail.split_at(i),
            None => (tail, ""),
        };
        let is_next = params.split(';').any(|p| {
            p.trim().strip_prefix("rel=").is_some_and(|v| {
                v.trim_matches(|c: char| c == '"' || c == ',' || c.is_whitespace())
                    .split_whitespace()
                    .any(|r| r == "next")
            })
        });
        if is_next {
            return Some(url.to_string());
        }
        rest = remaining;
    }
    None
}

#[derive(Deserialize)]
struct GhUser {
    id: i64,
    login: String,
    email: Option<String>,
}

#[derive(Deserialize)]
struct GhRepo {
    id: i64,
    name: String,
    default_branch: Option<String>,
    owner: GhOwner,
}

#[derive(Deserialize)]
struct GhOwner {
    login: String,
}

#[derive(Deserialize)]
struct GhHook {
    id: i64,
    #[serde(default)]
    active: bool,
    config: Option<GhHookConfig>,
}

#[derive(Deserialize)]
struct GhHookConfig {
    url: Option<String>,
}

impl GhHook {
    /// Sehat = aktif dan mengarah ke `url` yang diminta.
    fn is_healthy(&self, url: &str) -> bool {
        self.active && self.config.as_ref().and_then(|c| c.url.as_deref()) == Some(url)
    }
}

const HOOK_EVENTS: [&str; 3] = [
    "pull_request",
    "issue_comment",
    "pull_request_review_comment",
];

#[derive(Deserialize)]
struct GhPull {
    number: i64,
    title: String,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    head: GhRef,
    base: GhRef,
}

#[derive(Deserialize)]
struct GhRef {
    sha: String,
}

#[derive(Deserialize)]
struct GhCompare {
    merge_base_commit: GhRef,
    #[serde(default)]
    files: Vec<GhFile>,
}

#[derive(Deserialize)]
struct GhFile {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    patch: Option<String>,
}

impl GhFile {
    fn into_diff(self) -> FileDiff {
        let status = match self.status.as_str() {
            "added" | "copied" => FileStatus::Added,
            "removed" => FileStatus::Deleted,
            "renamed" => FileStatus::Renamed,
            // "modified", "changed", "unchanged" dan status baru yang tak dikenal.
            _ => FileStatus::Modified,
        };
        FileDiff {
            path: normalize_path(&self.filename),
            old_path: self.previous_filename.as_deref().map(normalize_path),
            status,
            patch: self.patch,
        }
    }
}

/// Comment author. `user` is null for deleted accounts.
#[derive(Deserialize)]
struct GhCommentUser {
    login: String,
    #[serde(rename = "type", default)]
    kind: String,
}

/// Shared shape of review comments, issue comments and reviews (unused fields absent).
#[derive(Deserialize)]
struct GhComment {
    id: i64,
    in_reply_to_id: Option<i64>,
    user: Option<GhCommentUser>,
    body: Option<String>,
    path: Option<String>,
    line: Option<u32>,
    html_url: Option<String>,
}

impl GhComment {
    fn is_bot(&self) -> bool {
        self.user.as_ref().is_some_and(|u| u.kind == "Bot")
    }

    fn into_comment(self) -> PrThreadComment {
        let body = self.body.unwrap_or_default();
        PrThreadComment {
            id: self.id.to_string(),
            // GitHub shows deleted accounts as `ghost`.
            author: self.user.map_or_else(|| "ghost".into(), |u| u.login),
            by_adus: is_adus_comment(&body),
            body,
        }
    }
}

/// A review comment id from a `ThreadRef`; anything but a number is an unknown thread.
fn comment_id(id: &str) -> Result<i64, GitError> {
    id.parse().map_err(|_| GitError::NotFound)
}

#[derive(Deserialize)]
struct GhReview {
    id: i64,
}

#[derive(Deserialize)]
struct GqlResponse {
    data: Option<Value>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: String,
}

/// `fullDatabaseId` is a `BigInt`, which GitHub encodes as a string (`databaseId` is deprecated).
const REVIEW_THREADS_QUERY: &str = "    query($owner: String!, $name: String!, $number: Int!, $after: String) {     repository(owner: $owner, name: $name) { pullRequest(number: $number) {     reviewThreads(first: 100, after: $after) {     nodes { id isResolved comments(first: 1) { nodes { fullDatabaseId } } }     pageInfo { hasNextPage endCursor } } } } }";
const RESOLVE_MUTATION: &str =
    "mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { id } } }";
const UNRESOLVE_MUTATION: &str =
    "mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { id } } }";

#[derive(Deserialize)]
struct GqlRepoData {
    repository: Option<GqlRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepository {
    pull_request: Option<GqlPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPull {
    review_threads: GqlThreads,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThreads {
    nodes: Vec<Option<GqlThread>>,
    page_info: GqlPageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThread {
    id: String,
    is_resolved: bool,
    comments: GqlNodes,
}

#[derive(Deserialize)]
struct GqlNodes {
    nodes: Vec<Option<GqlThreadComment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThreadComment {
    full_database_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn http_url(url: Option<String>) -> Option<String> {
    url.filter(|u| u.starts_with("https://") || u.starts_with("http://"))
}

/// One top-level thread per comment.
fn top_level_threads(comments: Vec<GhComment>) -> impl Iterator<Item = PrThread> {
    comments.into_iter().map(|c| PrThread {
        id: None,
        resolved: false,
        path: None,
        line: None,
        status: None,
        url: http_url(c.html_url.clone()),
        comments: vec![c.into_comment()],
    })
}

/// Groups review comments into threads by `in_reply_to_id` (GitHub points replies at the root).
/// A reply whose root is missing starts its own thread. Bot comments are dropped after
/// grouping, so a human reply to a bot root still lands in that thread (a thread may end up
/// empty). `resolved` is always `false`: REST has no resolution.
fn review_comment_threads(mut comments: Vec<GhComment>) -> Vec<PrThread> {
    comments.sort_by_key(|c| c.id);
    let mut threads: Vec<(i64, PrThread)> = Vec::new();
    for c in comments {
        let root = c
            .in_reply_to_id
            .and_then(|r| threads.iter().position(|(id, _)| *id == r));
        let i = match root {
            Some(i) => i,
            None => {
                threads.push((
                    c.id,
                    PrThread {
                        id: Some(c.id.to_string()),
                        resolved: false,
                        path: c.path.as_deref().map(normalize_path),
                        line: c.line,
                        status: None,
                        url: http_url(c.html_url.clone()),
                        comments: Vec::new(),
                    },
                ));
                threads.len() - 1
            }
        };
        if !c.is_bot() {
            threads[i].1.comments.push(c.into_comment());
        }
    }
    threads.into_iter().map(|(_, t)| t).collect()
}

#[async_trait]
impl GitProvider for GithubProvider {
    async fn verify(&self) -> Result<PatOwner, GitError> {
        let user: GhUser = self.get_json(&format!("{}/user", self.base_url)).await?;
        Ok(PatOwner {
            id: user.id.to_string(),
            login: user.login,
            email: user.email,
        })
    }

    async fn list_repos(&self) -> Result<Vec<RemoteRepo>, GitError> {
        let url = format!(
            "{}/user/repos?per_page=100&affiliation=owner,collaborator,organization_member",
            self.base_url
        );
        let pages: Vec<Vec<GhRepo>> = self.get_pages(url).await?;
        Ok(pages
            .into_iter()
            .flatten()
            .map(|r| RemoteRepo {
                r#ref: RepoRef {
                    external_id: r.id.to_string(),
                    owner: r.owner.login,
                    project: None,
                    name: r.name,
                },
                default_branch: r.default_branch,
                project_name: None,
            })
            .collect())
    }

    /// GitHub hanya butuh satu hook. Id di `existing` yang masih ada (non-404) dipertahankan;
    /// yang nonaktif atau mengarah ke URL lain di-PATCH (id tetap). Kalau tidak ada yang
    /// tersisa, dibuat satu hook baru. Tidak pernah membuat duplikat.
    async fn ensure_webhooks(
        &self,
        repo: &RepoRef,
        url: &str,
        secret: &str,
        existing: &[String],
    ) -> Result<Vec<String>, GitError> {
        let config = json!({ "url": url, "content_type": "json", "secret": secret });
        let mut alive = Vec::new();
        for id in existing {
            let hook_url = self.repo_url(repo, &format!("/hooks/{id}"));
            match self.get_json::<GhHook>(&hook_url).await {
                Ok(hook) if hook.is_healthy(url) => alive.push(id.clone()),
                Ok(_) => {
                    let body = json!({ "config": config, "events": HOOK_EVENTS, "active": true });
                    let req = self.with_headers(self.http.patch(&hook_url)).json(&body);
                    self.send(req).await?;
                    alive.push(id.clone());
                }
                Err(GitError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if alive.is_empty() {
            let body = json!({
                "name": "web",
                "active": true,
                "events": HOOK_EVENTS,
                "config": config,
            });
            let resp = self
                .send(self.post(&self.repo_url(repo, "/hooks")).json(&body))
                .await?;
            let hook: GhHook = decode(resp).await?;
            alive.push(hook.id.to_string());
        }
        Ok(alive)
    }

    /// 404 saat menghapus dianggap sukses (hook sudah hilang).
    async fn delete_webhooks(&self, repo: &RepoRef, ids: &[String]) -> Result<(), GitError> {
        for id in ids {
            let req = self.with_headers(
                self.http
                    .delete(self.repo_url(repo, &format!("/hooks/{id}"))),
            );
            match self.send(req).await {
                Ok(_) | Err(GitError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn webhooks_exist(
        &self,
        repo: &RepoRef,
        ids: &[String],
        url: &str,
    ) -> Result<bool, GitError> {
        for id in ids {
            match self
                .get_json::<GhHook>(&self.repo_url(repo, &format!("/hooks/{id}")))
                .await
            {
                Ok(hook) if hook.is_healthy(url) => {}
                // Nonaktif atau URL lama sama saja dengan hilang.
                Ok(_) | Err(GitError::NotFound) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    async fn get_pr(&self, repo: &RepoRef, number: i64) -> Result<PullRequest, GitError> {
        let pull: GhPull = self
            .get_json(&self.repo_url(repo, &format!("/pulls/{number}")))
            .await?;
        // `base.sha` adalah ujung branch target; yang dipakai review adalah merge base.
        let compare: GhCompare = self
            .get_json(&self.repo_url(
                repo,
                &format!("/compare/{}...{}?per_page=1", pull.base.sha, pull.head.sha),
            ))
            .await?;
        Ok(PullRequest {
            number: pull.number,
            title: pull.title,
            description: pull.body.unwrap_or_default(),
            head_sha: pull.head.sha,
            base_sha: compare.merge_base_commit.sha,
            is_draft: pull.draft,
            source_ref: None,
        })
    }

    async fn pr_diff(&self, repo: &RepoRef, pr: &PullRequest) -> Result<Vec<FileDiff>, GitError> {
        self.files_of(self.repo_url(repo, &format!("/pulls/{}/files?per_page=100", pr.number)))
            .await
    }

    async fn diff_between(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError> {
        let url = self.repo_url(
            repo,
            &format!("/compare/{base_sha}...{head_sha}?per_page=100"),
        );
        let pages: Vec<GhCompare> = self.get_pages(url).await?;
        Ok(pages
            .into_iter()
            .flat_map(|p| p.files)
            .map(GhFile::into_diff)
            .collect())
    }

    /// Selalu satu review `COMMENT` di commit `pr.head_sha`; daftar komentar kosong tetap
    /// mengirim review yang hanya berisi `summary`. Then reads the review's comments back for
    /// their ids; if that read fails the review stays posted and no ids are returned (an error
    /// would make the job retry and post the review twice).
    async fn post_review(
        &self,
        repo: &RepoRef,
        pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<Vec<PostedComment>, GitError> {
        let inline: Vec<_> = comments
            .iter()
            .map(|c| {
                let mut item = json!({
                    "path": c.path,
                    "line": c.line,
                    "side": "RIGHT",
                    "body": c.body,
                });
                if let Some(start) = c.start_line.filter(|&s| s < c.line)
                    && let Some(obj) = item.as_object_mut()
                {
                    obj.insert("start_line".into(), json!(start));
                    obj.insert("start_side".into(), json!("RIGHT"));
                }
                item
            })
            .collect();
        let body = json!({
            "commit_id": pr.head_sha,
            "event": "COMMENT",
            "body": summary,
            "comments": inline,
        });
        let url = self.repo_url(repo, &format!("/pulls/{}/reviews", pr.number));
        let review: GhReview = decode(self.send(self.post(&url).json(&body)).await?).await?;
        if comments.is_empty() {
            return Ok(Vec::new());
        }
        match self.posted_ids(repo, pr.number, review.id, comments).await {
            Ok(posted) => Ok(posted),
            Err(err) => {
                tracing::warn!(error = %err, "github review comments not read back");
                Ok(Vec::new())
            }
        }
    }

    async fn reply_in_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
        body: &str,
    ) -> Result<String, GitError> {
        let root = comment_id(&thread.root_comment_id)?;
        let url = self.repo_url(repo, &format!("/pulls/{pr_number}/comments/{root}/replies"));
        let req = self.post(&url).json(&json!({ "body": body }));
        let reply: GhComment = decode(self.send(req).await?).await?;
        Ok(reply.id.to_string())
    }

    /// Looks the GraphQL review-thread node id up every time (it is never stored), then
    /// resolves (`Fixed`/`WontFix`/`ByDesign`) or unresolves (`Active`).
    async fn set_thread_status(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
        status: ThreadStatus,
    ) -> Result<(), GitError> {
        let root = comment_id(&thread.root_comment_id)?;
        let (node_id, _) = self.review_thread(repo, pr_number, root).await?;
        let mutation = match status {
            ThreadStatus::Active => UNRESOLVE_MUTATION,
            ThreadStatus::Fixed | ThreadStatus::WontFix | ThreadStatus::ByDesign => {
                RESOLVE_MUTATION
            }
        };
        self.graphql::<serde::de::IgnoredAny>(mutation, json!({ "id": node_id }))
            .await?;
        Ok(())
    }

    /// The PR's review comments that are the root or reply to it, plus `isResolved` from
    /// GraphQL. Bot comments are left out, as in `list_pr_threads`.
    async fn get_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
    ) -> Result<PrThread, GitError> {
        let root = comment_id(&thread.root_comment_id)?;
        let pages: Vec<Vec<GhComment>> = self
            .get_pages(self.repo_url(repo, &format!("/pulls/{pr_number}/comments?per_page=100")))
            .await?;
        let own = pages
            .into_iter()
            .flatten()
            .filter(|c| c.id == root || c.in_reply_to_id == Some(root))
            .collect();
        let root_id = root.to_string();
        let mut found = review_comment_threads(own)
            .into_iter()
            .find(|t| t.id.as_deref() == Some(root_id.as_str()))
            .ok_or(GitError::NotFound)?;
        found.resolved = self.review_thread(repo, pr_number, root).await?.1;
        Ok(found)
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        body: &str,
    ) -> Result<(), GitError> {
        let url = self.repo_url(repo, &format!("/issues/{pr_number}/comments"));
        self.send(self.post(&url).json(&json!({ "body": body })))
            .await?;
        Ok(())
    }

    /// Inline review threads, then issue comments, then non-empty review bodies. GitHub REST
    /// has no thread status, so `status` is always `None`. Bot users are skipped.
    async fn list_pr_threads(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> Result<Vec<PrThread>, GitError> {
        let review: Vec<Vec<GhComment>> = self
            .get_pages(self.repo_url(repo, &format!("/pulls/{pr_number}/comments?per_page=100")))
            .await?;
        let issue: Vec<Vec<GhComment>> = self
            .get_pages(self.repo_url(repo, &format!("/issues/{pr_number}/comments?per_page=100")))
            .await?;
        let reviews: Vec<Vec<GhComment>> = self
            .get_pages(self.repo_url(repo, &format!("/pulls/{pr_number}/reviews?per_page=100")))
            .await?;
        let mut threads = review_comment_threads(review.into_iter().flatten().collect());
        threads.retain(|t| !t.comments.is_empty());
        let top = issue.into_iter().flatten().chain(
            reviews
                .into_iter()
                .flatten()
                .filter(|r| r.body.as_deref().is_some_and(|b| !b.trim().is_empty())),
        );
        threads.extend(top_level_threads(top.filter(|c| !c.is_bot()).collect()));
        Ok(threads)
    }

    /// `https://api.github.com` clones from `https://github.com`, GHES `{origin}/api/v3` from
    /// `{origin}`, any other base (test servers) from the base itself.
    fn clone_spec(&self, repo: &RepoRef, pr: &PullRequest) -> Option<CloneSpec> {
        let git_base = if self.base_url == "https://api.github.com" {
            "https://github.com"
        } else {
            self.base_url
                .strip_suffix("/api/v3")
                .unwrap_or(&self.base_url)
        };
        if repo.name.is_empty() {
            return None;
        }
        let url = git_url(git_base, &[&repo.owner, &format!("{}.git", repo.name)])?;
        Some(CloneSpec {
            url,
            refspec: format!("refs/pull/{}/head", pr.number),
            fallback_refspec: None,
            expected_sha: pr.head_sha.clone(),
            auth_header: basic_auth_header("x-access-token", &self.token),
        })
    }
}
