//! Perintah `@adudu` di komentar PR: `review` (jalankan ulang review penuh) dan
//! `remember: <isi>` (simpan memory repo). Hanya user Adus terdaftar yang boleh memerintah.

use adus_core::job::{CommentAuthor, Job, Trigger};
use adus_core::types::ConnectionKind;
use adus_db::connections::Connection;
use adus_db::memories::{self, MemorySource, NewMemory};
use adus_db::repositories;
use adus_db::rules::KnowledgeScope;
use adus_db::users::{self, User};
use adus_git::GitError;
use adus_queue::Queue;
use sqlx::PgPool;

use crate::MARKER;
use crate::engine::{Engine, EngineError, db_error};

const MENTION: &str = "@adudu";
/// Batas panjang isi `remember`, dalam karakter.
const MAX_MEMORY_CHARS: usize = 2000;
const HELP: &str = "I didn't understand that. I know these commands:\n\n\
                    - `@adudu review` — run a full review of this pull request\n\
                    - `@adudu remember: <text>` — save a convention for this repository";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Review,
    Remember(String),
    Unknown,
}

/// `None` jika tidak ada mention `@adudu` atau komentar itu buatan Adus sendiri (memuat MARKER).
/// Hanya teks setelah mention pertama yang dibaca; kata pertamanya adalah perintah.
pub fn parse_command(body: &str) -> Option<Command> {
    if body.contains(MARKER) {
        return None;
    }
    let rest = after_mention(body)?.trim_start();
    if word(rest, "review").is_some() {
        return Some(Command::Review);
    }
    let rest = word(rest, "please").map_or(rest, str::trim_start);
    let Some(text) = word(rest, "remember") else {
        return Some(Command::Unknown);
    };
    let text = text.strip_prefix(':').unwrap_or(text).trim();
    if text.is_empty() {
        return Some(Command::Unknown);
    }
    Some(Command::Remember(text.to_string()))
}

/// Teks setelah mention `@adudu` pertama yang berdiri sendiri: sebelumnya awal teks atau
/// bukan huruf/angka (`foo@adudu.com` bukan mention), sesudahnya akhir teks, spasi, atau
/// tanda baca selain `_`/`-` (`@aduduku`, `@adudu-bot` bukan mention).
fn after_mention(body: &str) -> Option<&str> {
    for (start, _) in body.match_indices('@') {
        let end = start + MENTION.len();
        // `MENTION` ASCII, jadi `end` pasti batas karakter jika cocok.
        let Some(candidate) = body.as_bytes().get(start..end) else {
            break;
        };
        if !candidate.eq_ignore_ascii_case(MENTION.as_bytes()) {
            continue;
        }
        let before_ok = body[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = body[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-'));
        if before_ok && after_ok {
            return Some(&body[end..]);
        }
    }
    None
}

/// Jika `text` diawali kata `word` (case-insensitive, utuh), kembalikan sisanya.
fn word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let head = text.get(..word.len())?;
    let tail = &text[word.len()..];
    let whole = tail.chars().next().is_none_or(|c| !c.is_alphanumeric());
    (head.eq_ignore_ascii_case(word) && whole).then_some(tail)
}

/// User Adus aktif yang menulis komentar. Azure dicocokkan lewat email; GitHub lewat
/// `github_login`, lalu email jika provider mengirimkannya.
pub async fn authorize(
    db: &PgPool,
    kind: ConnectionKind,
    author: &CommentAuthor,
) -> Result<Option<User>, sqlx::Error> {
    let mut user = None;
    if kind == ConnectionKind::Github
        && let Some(login) = &author.login
    {
        user = users::by_github_login(db, login).await?;
    }
    if user.is_none()
        && let Some(email) = &author.email
    {
        user = users::by_email(db, email).await?;
    }
    Ok(user.filter(|user| !user.disabled))
}

impl Engine {
    /// Jalankan perintah `@adudu` dari job `PrComment`. Penulis tak terdaftar diabaikan tanpa balasan.
    pub async fn handle_comment(&self, queue: &Queue, job: &Job) -> Result<(), EngineError> {
        let Job::PrComment {
            repository_id,
            pr_number,
            comment_url,
            author,
            body,
        } = job
        else {
            return Err(EngineError::Fatal("not a comment job".into()));
        };
        let Some(command) = parse_command(body) else {
            return Ok(());
        };
        let Some(repo) = repositories::get(&self.db, *repository_id)
            .await
            .map_err(db_error)?
        else {
            return Ok(());
        };
        let conn = self.connection(&repo).await?;
        let user = authorize(&self.db, conn.kind, author)
            .await
            .map_err(db_error)?;
        let Some(user) = user else {
            // Isi komentar sengaja tidak di-log.
            tracing::info!(%repository_id, pr_number, "ignoring @adudu command from an unregistered author");
            return Ok(());
        };
        let (git, repo_ref) = self.provider(&conn, &repo)?;

        // `done` = efek samping sudah terjadi; retry setelahnya akan menggandakannya.
        let (reply, done) = match command {
            Command::Review => {
                let review = Job::ReviewPr {
                    repository_id: *repository_id,
                    pr_number: *pr_number,
                    trigger: Trigger::Command,
                };
                queue
                    .enqueue(&review)
                    .await
                    .map_err(|err| EngineError::Retryable(format!("queue: {err}"), None))?;
                ("On it — starting a review.".to_string(), true)
            }
            Command::Remember(text) if text.chars().count() > MAX_MEMORY_CHARS => {
                let reply = format!("Memory is too long (max {MAX_MEMORY_CHARS} characters).");
                (reply, false)
            }
            Command::Remember(text) => {
                // Sama dengan penjaga di webhook: hanya URL http(s) yang disimpan sebagai tautan.
                let source_url = comment_url
                    .as_deref()
                    .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
                let new = NewMemory {
                    scope: KnowledgeScope::Repo,
                    repository_id: Some(repo.id),
                    content: &text,
                    source: MemorySource::PrComment,
                    source_url,
                    created_by: Some(user.id),
                };
                memories::insert(&self.db, new).await.map_err(db_error)?;
                let quoted: Vec<String> = text.lines().map(|line| format!("> {line}")).collect();
                let reply = format!(
                    "Got it — I'll remember this for this repository:\n\n{}",
                    quoted.join("\n")
                );
                (reply, true)
            }
            Command::Unknown => (HELP.to_string(), false),
        };

        let body = format!("{MARKER}\n{reply}");
        let Err(err) = git.post_comment(&repo_ref, *pr_number, &body).await else {
            return Ok(());
        };
        let err = self.reply_error(&conn, err).await;
        if done {
            tracing::warn!(%repository_id, pr_number, error = %err, "command ran but the reply could not be posted");
            return Ok(());
        }
        Err(err)
    }

    /// Token ditolak → connection `invalid` dan fatal; rate limit / HTTP → boleh diulang.
    async fn reply_error(&self, conn: &Connection, err: GitError) -> EngineError {
        let message = format!("posting the reply failed: {err}");
        match err {
            GitError::Unauthorized => self.unauthorized(conn).await,
            GitError::RateLimited | GitError::Http(_) | GitError::Status(_) => {
                EngineError::Retryable(message, None)
            }
            GitError::NotFound | GitError::Decode(_) => EngineError::Fatal(message),
        }
    }
}
