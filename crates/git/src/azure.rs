//! Provider Azure DevOps Services (REST 7.1) dengan PAT.
//!
//! `RepoRef` untuk Azure: `owner` = organization, `project` = GUID project, `name` = nama repo,
//! `external_id` = GUID repo. Semua URL API memakai GUID (Azure menerima id sebagai ganti nama).

use async_trait::async_trait;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::diff::unified_patch;
use crate::{
    CloneSpec, FileDiff, FileStatus, GitError, GitProvider, InlineComment, PatOwner, PullRequest,
    RemoteRepo, RepoRef, basic_auth_header, decode, git_url, normalize_path,
};

const API_VERSION: &str = "7.1";
/// Batas halaman `$top`/`$skip` (30 x 100 = 3000 perubahan).
const MAX_PAGES: usize = 30;
const PAGE_SIZE: usize = 100;
/// Satu subscription per event.
const EVENTS: [&str; 3] = [
    "git.pullrequest.created",
    "git.pullrequest.updated",
    "ms.vss-code.git-pullrequest-comment-event",
];

pub struct AzureProvider {
    http: reqwest::Client,
    base_url: String,
    organization: String,
    token: String,
}

impl AzureProvider {
    /// `http` dibuat pemanggil (timeout diatur di sana). `base_url` contoh `https://dev.azure.com`.
    pub fn new(
        http: reqwest::Client,
        base_url: String,
        organization: String,
        token: String,
    ) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            organization,
            token,
        }
    }

    fn org_url(&self, rest: &str) -> String {
        format!("{}/{}/_apis{}", self.base_url, self.organization, rest)
    }

    /// URL di bawah repo. Memakai GUID project dan GUID repo; `project` wajib.
    fn repo_url(&self, repo: &RepoRef, rest: &str) -> Result<String, GitError> {
        let project = repo
            .project
            .as_deref()
            .ok_or_else(|| GitError::Http("missing project".into()))?;
        Ok(format!(
            "{}/{}/{}/_apis/git/repositories/{}{}",
            self.base_url, self.organization, project, repo.external_id, rest
        ))
    }

    /// PAT dikirim sebagai Basic auth dengan username kosong: `base64(":" + PAT)`.
    fn with_headers(&self, req: RequestBuilder) -> RequestBuilder {
        req.basic_auth("", Some(&self.token))
            .header("User-Agent", "adus")
    }

    fn get(&self, url: &str) -> RequestBuilder {
        self.with_headers(self.http.get(url))
            .query(&[("api-version", API_VERSION)])
    }

    fn post(&self, url: &str) -> RequestBuilder {
        self.with_headers(self.http.post(url))
            .query(&[("api-version", API_VERSION)])
    }

    fn delete(&self, url: &str) -> RequestBuilder {
        self.with_headers(self.http.delete(url))
            .query(&[("api-version", API_VERSION)])
    }

    /// Kirim request lalu petakan status gagal ke `GitError`. Token, URL, dan body tidak pernah
    /// masuk pesan error.
    async fn send(&self, req: RequestBuilder) -> Result<Response, GitError> {
        let resp = req
            .send()
            .await
            .map_err(|e| GitError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        // Azure menjawab PAT salah/kedaluwarsa dengan 203 + halaman HTML sign-in.
        if status == StatusCode::NON_AUTHORITATIVE_INFORMATION {
            return Err(GitError::Unauthorized);
        }
        if status.is_success() {
            return Ok(resp);
        }
        Err(match status {
            StatusCode::UNAUTHORIZED => GitError::Unauthorized,
            StatusCode::TOO_MANY_REQUESTS => GitError::RateLimited,
            StatusCode::NOT_FOUND => GitError::NotFound,
            other => GitError::Status(other.as_u16()),
        })
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, GitError> {
        decode(self.send(self.get(url)).await?).await
    }

    /// Semua iterasi PR. Tiap iterasi punya merge base (`commonRefCommit`) dan head (`sourceRefCommit`).
    async fn iterations(&self, repo: &RepoRef, number: i64) -> Result<Vec<AzIteration>, GitError> {
        let list: AzList<AzIteration> = self
            .get_json(&self.repo_url(repo, &format!("/pullRequests/{number}/iterations"))?)
            .await?;
        Ok(list.value)
    }

    /// Ambil semua perubahan lewat `$top`/`$skip` (maks `MAX_PAGES`). Berlanjut selama respons
    /// bilang masih ada: `nextSkip`/`nextTop` > 0 (iterasi PR) atau `allChangesIncluded == false`
    /// (diff commit).
    async fn get_changes(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<AzChange>, GitError> {
        let mut all: Vec<AzChange> = Vec::new();
        for page in 1..=MAX_PAGES {
            let req = self
                .get(url)
                .query(params)
                .query(&[("$top", PAGE_SIZE)])
                .query(&[("$skip", all.len())]);
            let body: AzChanges = decode(self.send(req).await?).await?;
            let got = body.changes.len();
            let more =
                body.next_skip > 0 || body.next_top > 0 || body.all_changes_included == Some(false);
            all.extend(body.changes);
            if !more || got == 0 {
                return Ok(all);
            }
            if page == MAX_PAGES {
                tracing::warn!(max_pages = MAX_PAGES, "azure changes pagination truncated");
            }
        }
        Ok(all)
    }

    /// Isi file pada commit `sha`. 404 = `NotFound`: pemanggil hanya meminta sisi yang seharusnya
    /// ada, jadi 404 berarti GUID/sha/path salah, bukan file kosong.
    async fn content(&self, repo: &RepoRef, path: &str, sha: &str) -> Result<Vec<u8>, GitError> {
        let slash_path = format!("/{path}");
        let req = self.get(&self.repo_url(repo, "/items")?).query(&[
            ("path", slash_path.as_str()),
            ("versionDescriptor.version", sha),
            ("versionDescriptor.versionType", "commit"),
            ("$format", "octetStream"),
        ]);
        match self.send(req).await {
            Ok(resp) => resp
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| GitError::Http(e.without_url().to_string())),
            Err(e) => Err(e),
        }
    }

    /// Ubah daftar perubahan jadi `FileDiff` dengan mengambil isi file di `base_sha` dan `head_sha`.
    // ponytail: isi file diambil berurutan, 2 request per file (maks 3000 file = 6000 request).
    // Naikkan ke `buffer_unordered(8)` kalau PR besar terasa lambat.
    async fn with_patches(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
        changes: Vec<AzChange>,
    ) -> Result<Vec<FileDiff>, GitError> {
        let mut out = Vec::new();
        for c in changes {
            // Lewati folder, submodule (`commit`), dan entri sisi-lama rename (`sourceRename`).
            let object_type = c.item.git_object_type.as_deref();
            if c.item.is_folder
                || matches!(object_type, Some("tree" | "commit"))
                || c.has_change_type("sourceRename")
            {
                continue;
            }
            let status = c.status();
            let path = normalize_path(&c.item.path);
            let old_path = match status {
                FileStatus::Renamed => c.original_path.as_deref().map(normalize_path),
                _ => None,
            };
            let old_src = old_path.as_deref().unwrap_or(&path);
            let old = match status {
                FileStatus::Added => Vec::new(),
                _ => self.content(repo, old_src, base_sha).await?,
            };
            let new = match status {
                FileStatus::Deleted => Vec::new(),
                _ => self.content(repo, &path, head_sha).await?,
            };
            // Bukan UTF-8 = biner, tanpa patch.
            let patch = match (String::from_utf8(old), String::from_utf8(new)) {
                (Ok(old), Ok(new)) => Some(unified_patch(&old, &new)),
                _ => None,
            };
            out.push(FileDiff {
                path,
                old_path,
                status,
                patch,
            });
        }
        Ok(out)
    }

    async fn post_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        body: &Value,
    ) -> Result<(), GitError> {
        let url = self.repo_url(repo, &format!("/pullRequests/{pr_number}/threads"))?;
        self.send(self.post(&url).json(body)).await?;
        Ok(())
    }
}

/// Thread status `1` = active; `commentType` 1 = text.
fn thread_body(content: &str, context: Option<Value>) -> Value {
    let mut body = json!({
        "comments": [{ "content": content, "commentType": 1 }],
        "status": 1,
    });
    if let Some(ctx) = context {
        body["threadContext"] = ctx;
    }
    body
}

#[derive(Deserialize)]
struct AzList<T> {
    value: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionData {
    authenticated_user: AuthUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthUser {
    id: String,
    provider_display_name: String,
    #[serde(default)]
    properties: AuthProps,
}

#[derive(Default, Deserialize)]
struct AuthProps {
    #[serde(rename = "Account")]
    account: Option<AuthAccount>,
}

#[derive(Deserialize)]
struct AuthAccount {
    #[serde(rename = "$value")]
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzRepo {
    id: String,
    name: String,
    default_branch: Option<String>,
    project: AzProject,
}

#[derive(Deserialize)]
struct AzProject {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzPull {
    pull_request_id: i64,
    title: String,
    description: Option<String>,
    #[serde(default)]
    is_draft: bool,
    last_merge_source_commit: AzCommit,
    source_ref_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzCommit {
    commit_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzIteration {
    id: i64,
    common_ref_commit: AzCommit,
    source_ref_commit: AzCommit,
}

/// Respons `iterations/{n}/changes` (`changeEntries`) dan `diffs/commits` (`changes`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzChanges {
    #[serde(default, alias = "changeEntries")]
    changes: Vec<AzChange>,
    #[serde(default)]
    next_skip: i64,
    #[serde(default)]
    next_top: i64,
    all_changes_included: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzChange {
    change_type: String,
    item: AzItem,
    original_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzItem {
    path: String,
    #[serde(default)]
    is_folder: bool,
    git_object_type: Option<String>,
}

impl AzChange {
    /// `changeType` bisa daftar seperti "edit, rename". Dicocokkan per kata (tanpa huruf besar/kecil)
    /// supaya "undelete" tidak dianggap "delete".
    fn has_change_type(&self, name: &str) -> bool {
        self.change_type
            .split(',')
            .any(|t| t.trim().eq_ignore_ascii_case(name))
    }

    /// `rename` dicek sebelum `add`: "add, rename" adalah file hasil pindah, bukan file baru.
    fn status(&self) -> FileStatus {
        if self.has_change_type("delete") {
            FileStatus::Deleted
        } else if self.has_change_type("rename") {
            FileStatus::Renamed
        } else if self.has_change_type("add") {
            FileStatus::Added
        } else {
            FileStatus::Modified
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzSubscription {
    id: String,
    event_type: String,
    status: Option<String>,
    consumer_inputs: Option<AzConsumerInputs>,
}

#[derive(Deserialize)]
struct AzConsumerInputs {
    url: Option<String>,
}

impl AzSubscription {
    /// Sehat = aktif dan mengarah ke `url` yang diminta. `disabledBySystem`/`disabledByUser`
    /// atau URL lain dianggap hilang.
    fn is_healthy(&self, url: &str) -> bool {
        let same_url = self.consumer_inputs.as_ref().and_then(|c| c.url.as_deref()) == Some(url);
        self.is_active() && same_url
    }

    fn is_active(&self) -> bool {
        self.status.as_deref().is_some_and(|s| {
            s.eq_ignore_ascii_case("enabled") || s.eq_ignore_ascii_case("onProbation")
        })
    }
}

#[async_trait]
impl GitProvider for AzureProvider {
    async fn verify(&self) -> Result<PatOwner, GitError> {
        // `connectionData` adalah endpoint legacy: dipanggil tanpa `api-version`.
        let url = self.org_url("/connectionData");
        let data: ConnectionData =
            decode(self.send(self.with_headers(self.http.get(&url))).await?).await?;
        let user = data.authenticated_user;
        Ok(PatOwner {
            id: user.id,
            login: user.provider_display_name,
            email: user.properties.account.and_then(|a| a.value),
        })
    }

    async fn list_repos(&self) -> Result<Vec<RemoteRepo>, GitError> {
        let list: AzList<AzRepo> = self.get_json(&self.org_url("/git/repositories")).await?;
        Ok(list
            .value
            .into_iter()
            .map(|r| RemoteRepo {
                project_name: Some(r.project.name),
                r#ref: RepoRef {
                    external_id: r.id,
                    owner: self.organization.clone(),
                    project: Some(r.project.id),
                    name: r.name,
                },
                default_branch: r
                    .default_branch
                    .map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_string()),
            })
            .collect())
    }

    /// Satu subscription per event. Id di `existing` yang masih ada (non-404) dipertahankan;
    /// hanya event yang belum punya subscription sehat yang dibuat. Azure tidak memakai `secret`.
    async fn ensure_webhooks(
        &self,
        repo: &RepoRef,
        url: &str,
        _secret: &str,
        existing: &[String],
    ) -> Result<Vec<String>, GitError> {
        let project = repo
            .project
            .as_deref()
            .ok_or_else(|| GitError::Http("missing project".into()))?;
        let mut alive = Vec::new();
        let mut covered: Vec<String> = Vec::new();
        for id in existing {
            match self
                .get_json::<AzSubscription>(&self.org_url(&format!("/hooks/subscriptions/{id}")))
                .await
            {
                Ok(sub) if sub.is_healthy(url) => {
                    covered.push(sub.event_type);
                    alive.push(id.clone());
                }
                // Nonaktif atau salah URL: hapus lalu dibuat ulang di bawah.
                Ok(_) => self.delete_webhooks(repo, std::slice::from_ref(id)).await?,
                Err(GitError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        for event in EVENTS {
            if covered.iter().any(|c| c == event) {
                continue;
            }
            let body = json!({
                "publisherId": "tfs",
                "eventType": event,
                "consumerId": "webHooks",
                "consumerActionId": "httpRequest",
                "publisherInputs": { "projectId": project, "repository": repo.external_id },
                "consumerInputs": { "url": url },
            });
            let resp = self
                .send(self.post(&self.org_url("/hooks/subscriptions")).json(&body))
                .await?;
            let sub: AzSubscription = decode(resp).await?;
            alive.push(sub.id);
        }
        Ok(alive)
    }

    /// 404 saat menghapus dianggap sukses (subscription sudah hilang).
    async fn delete_webhooks(&self, _repo: &RepoRef, ids: &[String]) -> Result<(), GitError> {
        for id in ids {
            let req = self.delete(&self.org_url(&format!("/hooks/subscriptions/{id}")));
            match self.send(req).await {
                Ok(_) | Err(GitError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn webhooks_exist(
        &self,
        _repo: &RepoRef,
        ids: &[String],
        url: &str,
    ) -> Result<bool, GitError> {
        for id in ids {
            match self
                .get_json::<AzSubscription>(&self.org_url(&format!("/hooks/subscriptions/{id}")))
                .await
            {
                Ok(sub) if sub.is_healthy(url) => {}
                // Nonaktif atau URL lama sama saja dengan hilang.
                Ok(_) | Err(GitError::NotFound) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    async fn get_pr(&self, repo: &RepoRef, number: i64) -> Result<PullRequest, GitError> {
        let pull: AzPull = self
            .get_json(&self.repo_url(repo, &format!("/pullrequests/{number}"))?)
            .await?;
        let iteration = self
            .iterations(repo, number)
            .await?
            .into_iter()
            .max_by_key(|i| i.id)
            .ok_or_else(|| GitError::Decode("pull request has no iterations".into()))?;
        Ok(PullRequest {
            number: pull.pull_request_id,
            title: pull.title,
            description: pull.description.unwrap_or_default(),
            head_sha: pull.last_merge_source_commit.commit_id,
            base_sha: iteration.common_ref_commit.commit_id,
            is_draft: pull.is_draft,
            source_ref: pull.source_ref_name,
        })
    }

    /// Memakai iterasi yang `sourceRefCommit`-nya = `pr.head_sha` (fallback: iterasi terbaru) dan
    /// mengambil isi file di commit milik iterasi itu, supaya daftar perubahan dan isi file
    /// konsisten walau ada push di antara `get_pr` dan `pr_diff`.
    async fn pr_diff(&self, repo: &RepoRef, pr: &PullRequest) -> Result<Vec<FileDiff>, GitError> {
        let mut iterations = self.iterations(repo, pr.number).await?;
        iterations.sort_by_key(|i| i.id);
        let iteration = iterations
            .iter()
            .rfind(|i| i.source_ref_commit.commit_id == pr.head_sha)
            .or(iterations.last())
            .ok_or_else(|| GitError::Decode("pull request has no iterations".into()))?;
        let url = self.repo_url(
            repo,
            &format!(
                "/pullRequests/{}/iterations/{}/changes",
                pr.number, iteration.id
            ),
        )?;
        let changes = self.get_changes(&url, &[("$compareTo", "0")]).await?;
        let base_sha = &iteration.common_ref_commit.commit_id;
        let head_sha = &iteration.source_ref_commit.commit_id;
        self.with_patches(repo, base_sha, head_sha, changes).await
    }

    async fn diff_between(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError> {
        let url = self.repo_url(repo, "/diffs/commits")?;
        let params = [
            ("baseVersion", base_sha),
            ("baseVersionType", "commit"),
            ("targetVersion", head_sha),
            ("targetVersionType", "commit"),
        ];
        let changes = self.get_changes(&url, &params).await?;
        self.with_patches(repo, base_sha, head_sha, changes).await
    }

    /// Satu thread per komentar inline (path diberi `/` di depan), lalu satu thread ringkasan
    /// tanpa `threadContext`. Return jumlah thread inline.
    async fn post_review(
        &self,
        repo: &RepoRef,
        pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<usize, GitError> {
        for c in comments {
            let (start_line, end_offset) = match c.start_line.filter(|&s| s < c.line) {
                Some(start) => (start, c.end_offset.filter(|&o| o >= 1).unwrap_or(1)),
                None => (c.line, 1),
            };
            let context = json!({
                "filePath": format!("/{}", normalize_path(&c.path)),
                "rightFileStart": { "line": start_line, "offset": 1 },
                "rightFileEnd": { "line": c.line, "offset": end_offset },
            });
            self.post_thread(repo, pr.number, &thread_body(&c.body, Some(context)))
                .await?;
        }
        self.post_thread(repo, pr.number, &thread_body(summary, None))
            .await?;
        Ok(comments.len())
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        body: &str,
    ) -> Result<(), GitError> {
        self.post_thread(repo, pr_number, &thread_body(body, None))
            .await
    }

    /// Fetches the head commit by SHA; servers that refuse a bare SHA get the source branch.
    fn clone_spec(&self, repo: &RepoRef, pr: &PullRequest) -> Option<CloneSpec> {
        let project = repo.project.as_deref()?;
        let url = git_url(
            &self.base_url,
            &[&self.organization, project, "_git", &repo.name],
        )?;
        Some(CloneSpec {
            url,
            refspec: pr.head_sha.clone(),
            fallback_refspec: pr.source_ref.clone(),
            expected_sha: pr.head_sha.clone(),
            auth_header: basic_auth_header("", &self.token),
        })
    }
}
