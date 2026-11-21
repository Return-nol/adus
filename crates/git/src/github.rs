//! Provider GitHub (REST v3) dengan PAT.

use async_trait::async_trait;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{
    CloneSpec, FileDiff, FileStatus, GitError, GitProvider, InlineComment, PatOwner, PullRequest,
    RemoteRepo, RepoRef, basic_auth_header, git_url, normalize_path,
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

    async fn files_of(&self, first_url: String) -> Result<Vec<FileDiff>, GitError> {
        let pages: Vec<Vec<GhFile>> = self.get_pages(first_url).await?;
        Ok(pages.into_iter().flatten().map(GhFile::into_diff).collect())
    }
}

async fn decode<T: DeserializeOwned>(resp: Response) -> Result<T, GitError> {
    resp.json::<T>()
        .await
        .map_err(|e| GitError::Decode(e.without_url().to_string()))
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
    /// mengirim review yang hanya berisi `summary`. Return jumlah komentar inline.
    async fn post_review(
        &self,
        repo: &RepoRef,
        pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<usize, GitError> {
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
        self.send(self.post(&url).json(&body)).await?;
        Ok(comments.len())
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
