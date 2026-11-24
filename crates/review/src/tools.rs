//! Read-only repository tools for the review agents (`read_file`, `grep`, `list_dir`,
//! `find_files`), run against a [`RepoWorkspace`] with a per-review [`ToolCache`].
//!
//! Arguments come from an LLM reading untrusted PR code, so every path goes through
//! [`RepoWorkspace`] validation, walks skip `.git` and secret files, and every result is capped.
//! Failures are returned to the model as `Error: ...` text so it can adjust its next call.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use adus_core::types::normalize_path;
use adus_git::{FileDiff, FileStatus};
use adus_llm::ToolDef;
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::prompt::{MAX_EXPAND_FILE_CHARS, diff_block, expand_diff_block, truncate_with_marker};
use crate::redact::redact_secrets;
use crate::workspace::{PathError, RepoPath, RepoWorkspace, is_secret_path};

/// Files larger than this are never read.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// A NUL byte in this many leading bytes marks a file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const READ_MAX_LINES: u32 = 200;
const READ_MAX_CHARS: usize = 8_000;
const GREP_MAX_GROUPS: usize = 40;
const GREP_MAX_CHARS: usize = 8_000;
const GREP_CONTEXT_LINES: usize = 2;
const GREP_MAX_FILES: usize = 20_000;
/// Longest line `grep` prints; longer lines (minified code) are cut.
const GREP_MAX_LINE_CHARS: usize = 500;
const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;
const LIST_MAX_ENTRIES: usize = 150;
const LIST_MAX_CHARS: usize = 4_000;
/// Room kept for the trailing truncation notes under a character cap.
const NOTE_RESERVE_CHARS: usize = 300;
const LINE_TRUNCATED: &str = " [line truncated]";
/// Longest model-supplied text (pattern, tool name, parser message) echoed back.
const MAX_ECHO_CHARS: usize = 200;
/// Wall-clock budget of one `grep` or `find_files` call. The 10 MB regex size limit bounds
/// memory, not time: a large pattern can push the regex engine to its slow O(pattern × text)
/// path, so the walk and the line loop stop when this runs out.
const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(10);
/// `grep` matches only this many leading bytes of a line, bounding the cost of the one regex
/// call a deadline check cannot interrupt.
const GREP_LINE_WINDOW: usize = 64 * 1024;

/// Time and per-line bounds of the search tools. Fields so tests can shrink them.
#[derive(Debug, Clone, Copy)]
struct SearchLimits {
    time_budget: Duration,
    line_window: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            time_budget: SEARCH_TIME_BUDGET,
            line_window: GREP_LINE_WINDOW,
        }
    }
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    case_sensitive: Option<bool>,
}

#[derive(Default, Deserialize)]
struct ListDirArgs {
    path: Option<String>,
}

#[derive(Deserialize)]
struct FindFilesArgs {
    pattern: String,
    path: Option<String>,
}

/// What a tool call returns to the model. Error content always starts with `"Error: "`,
/// because some providers drop the error flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(content: String) -> Self {
        Self {
            content,
            is_error: false,
        }
    }

    fn error(err: &ToolError) -> Self {
        Self {
            content: format!("Error: {err}"),
            is_error: true,
        }
    }
}

/// Why a tool call failed. The text is shown to the model, so it says how to adjust and never
/// contains absolute paths.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    #[error("unknown tool `{0}`; available tools: read_file, grep, list_dir, find_files")]
    UnknownTool(String),
    #[error(
        "the arguments are not valid JSON ({0}); resend the call with a JSON object that matches the tool schema"
    )]
    UnparsableArgs(String),
    #[error("invalid arguments for {tool}: {message}; check the tool schema")]
    InvalidArgs { tool: &'static str, message: String },
    #[error("{}", path_error_message(.0))]
    Path(#[from] PathError),
    #[error("{0} not found; use list_dir or find_files to locate files")]
    NotFound(String),
    #[error("{0} is a directory; use list_dir to see its entries")]
    IsDirectory(String),
    #[error("{0} is not a directory; use read_file for files")]
    NotADirectory(String),
    #[error("file exceeds 2 MB limit; use grep instead")]
    TooLarge,
    #[error("binary file; only text files can be read, so read a source or text file instead")]
    Binary,
    #[error("cannot read the file ({0}); try another path or use list_dir to check it")]
    Io(#[from] std::io::Error),
    #[error("start_line {start} is past the end of {path} ({total} lines)")]
    PastEnd {
        path: String,
        start: u32,
        total: usize,
    },
    #[error("end_line must not be smaller than start_line")]
    BadRange,
    #[error("pattern must not be empty; pass a regular expression such as fn\\s+main")]
    EmptyPattern,
    #[error("invalid regex: {0}; simplify the pattern or escape special characters")]
    Regex(String),
    #[error("invalid glob pattern: {0}; use *, ** and {{a,b}} with / separators")]
    Glob(String),
}

fn path_error_message(err: &PathError) -> String {
    match err {
        PathError::Invalid(_) | PathError::EscapesRoot => {
            format!("{err}; use a path relative to the repository root, such as src/main.rs")
        }
        PathError::ForbiddenGitPath | PathError::SecretFile => format!("{err}; skip this path"),
    }
}

/// A read-only call with normalized arguments. Doubles as the [`ToolCache`] key, so calls that
/// differ only in spelling (`./src/a.rs`, default flags, an oversized line window) share a result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    ReadFile {
        path: String,
        start: u32,
        end: u32,
    },
    Grep {
        pattern: String,
        path: String,
        glob: Option<String>,
        case_sensitive: bool,
    },
    ListDir {
        path: String,
    },
    FindFiles {
        pattern: String,
        path: String,
    },
}

/// Results of successful read-only calls, shared by every agent of one review. The lock is
/// only taken inside blocking tool work, never across an `.await`.
#[derive(Debug, Default)]
struct ToolCache {
    entries: RwLock<HashMap<CacheKey, String>>,
    hits: AtomicU64,
    /// Fresh results that had a secret masked (cache hits are not counted again).
    redacted: AtomicU64,
}

impl ToolCache {
    /// How many calls were answered from the cache.
    #[cfg(test)]
    fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// How many fresh tool results had a secret masked.
    fn redacted(&self) -> u64 {
        self.redacted.load(Ordering::Relaxed)
    }

    fn get(&self, key: &CacheKey) -> Option<String> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let found = entries.get(key).cloned();
        if found.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    fn insert(&self, key: CacheKey, value: String) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, value);
    }
}

/// A parsed read-only tool call.
enum ReadOnlyCall {
    ReadFile(ReadFileArgs),
    Grep(GrepArgs),
    ListDir(ListDirArgs),
    FindFiles(FindFilesArgs),
}

impl ReadOnlyCall {
    fn parse(name: &str, args: &Value) -> Result<Self, ToolError> {
        // OpenAI hands back arguments it could not parse in this shape (see `adus_llm`).
        if let Some(reason) = args.get("__raw_parse_error") {
            let reason = reason.as_str().unwrap_or("malformed arguments");
            return Err(ToolError::UnparsableArgs(excerpt(reason, MAX_ECHO_CHARS)));
        }
        let empty = json!({});
        let args = if args.is_null() { &empty } else { args };
        match name {
            "read_file" => parse_args("read_file", args).map(Self::ReadFile),
            "grep" => parse_args("grep", args).map(Self::Grep),
            "list_dir" => parse_args("list_dir", args).map(Self::ListDir),
            "find_files" => parse_args("find_files", args).map(Self::FindFiles),
            other => Err(ToolError::UnknownTool(excerpt(other, MAX_ECHO_CHARS))),
        }
    }

    /// Resolves paths and defaults into `(CacheKey, RepoPath)`. Touches the filesystem once.
    fn normalize(&self, ws: &RepoWorkspace) -> Result<(CacheKey, RepoPath), ToolError> {
        let resolve_opt = |path: Option<&str>| -> Result<RepoPath, ToolError> {
            Ok(ws.resolve_or_root(path.map_or("", str::trim))?)
        };
        match self {
            Self::ReadFile(args) => {
                let path = ws.resolve(args.path.trim())?;
                let start = args.start_line.unwrap_or(1).max(1);
                let last_allowed = start.saturating_add(READ_MAX_LINES - 1);
                let end = match args.end_line {
                    Some(end) if end < start => return Err(ToolError::BadRange),
                    Some(end) => end.min(last_allowed),
                    None => last_allowed,
                };
                let key = CacheKey::ReadFile {
                    path: path.rel().to_owned(),
                    start,
                    end,
                };
                Ok((key, path))
            }
            Self::Grep(args) => {
                if args.pattern.is_empty() {
                    return Err(ToolError::EmptyPattern);
                }
                let path = resolve_opt(args.path.as_deref())?;
                let key = CacheKey::Grep {
                    pattern: args.pattern.clone(),
                    path: path.rel().to_owned(),
                    glob: args
                        .glob
                        .as_deref()
                        .map(str::trim)
                        .filter(|g| !g.is_empty())
                        .map(str::to_owned),
                    case_sensitive: args.case_sensitive.unwrap_or(true),
                };
                Ok((key, path))
            }
            Self::ListDir(args) => {
                let path = resolve_opt(args.path.as_deref())?;
                let key = CacheKey::ListDir {
                    path: path.rel().to_owned(),
                };
                Ok((key, path))
            }
            Self::FindFiles(args) => {
                let path = resolve_opt(args.path.as_deref())?;
                let key = CacheKey::FindFiles {
                    pattern: args.pattern.trim().to_owned(),
                    path: path.rel().to_owned(),
                };
                Ok((key, path))
            }
        }
    }
}

fn parse_args<T: DeserializeOwned>(tool: &'static str, args: &Value) -> Result<T, ToolError> {
    T::deserialize(args).map_err(|e| ToolError::InvalidArgs {
        tool,
        message: e.to_string(),
    })
}

static READ_ONLY_DEFS: LazyLock<Vec<ToolDef>> = LazyLock::new(read_only_defs);

/// The read-only tools bound to one workspace and cache.
#[derive(Debug)]
pub(crate) struct Toolbox {
    workspace: Arc<RepoWorkspace>,
    cache: Arc<ToolCache>,
    limits: SearchLimits,
}

impl Toolbox {
    pub(crate) fn new(workspace: Arc<RepoWorkspace>) -> Self {
        Self {
            workspace,
            cache: Arc::new(ToolCache::default()),
            limits: SearchLimits::default(),
        }
    }

    /// How many fresh tool results of this review had a secret masked.
    pub(crate) fn redacted_outputs(&self) -> u64 {
        self.cache.redacted()
    }

    /// Definitions of `read_file`, `grep`, `list_dir` and `find_files`, in that order.
    pub(crate) fn read_only_tool_defs(&self) -> &[ToolDef] {
        &READ_ONLY_DEFS
    }

    /// Runs one read-only tool call off the async runtime. Never panics; failures come back as
    /// `is_error` outputs.
    pub(crate) async fn execute_read_only(&self, name: &str, args: &Value) -> ToolOutput {
        let call = match ReadOnlyCall::parse(name, args) {
            Ok(call) => call,
            Err(err) => return ToolOutput::error(&err),
        };
        let workspace = Arc::clone(&self.workspace);
        let cache = Arc::clone(&self.cache);
        let limits = self.limits;
        let task =
            tokio::task::spawn_blocking(move || run_cached(&workspace, &cache, &call, limits));
        match task.await {
            Ok(Ok(content)) => ToolOutput::ok(content),
            Ok(Err(err)) => ToolOutput::error(&err),
            Err(join) => {
                tracing::error!(tool = name, error = %join, "read-only tool task failed");
                ToolOutput {
                    content: "Error: the tool failed unexpectedly; try a narrower request".into(),
                    is_error: true,
                }
            }
        }
    }

    /// [`read_file_snippet_sync`] run off the async runtime (`spawn_blocking`).
    pub(crate) async fn read_file_snippet(
        &self,
        path: &str,
        center_line: u32,
        radius: u32,
    ) -> Option<String> {
        let ws = Arc::clone(&self.workspace);
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || read_file_snippet_sync(&ws, &path, center_line, radius))
            .await
            .ok()
            .flatten()
    }
}

/// Lines `center_line ± radius` of `path` in the `read_file` format, or `None` on any
/// failure (bad path, secret, binary, too large, line out of range). Blocking.
///
/// The text is raw: never redacted and never cached, so the Verifier sees a hardcoded
/// secret the finding is about.
fn read_file_snippet_sync(
    ws: &RepoWorkspace,
    path: &str,
    center_line: u32,
    radius: u32,
) -> Option<String> {
    let target = ws.resolve(path.trim()).ok()?;
    let start = center_line.saturating_sub(radius).max(1);
    let end = center_line
        .saturating_add(radius)
        .min(start.saturating_add(READ_MAX_LINES - 1));
    read_file(&target, start, end).ok()
}

/// Pre-seeds up to [`crate::prompt::EXPANDED_HUNK_CONTEXT`] surrounding head-file lines around
/// each hunk for modified/renamed files whose raw diff is under [`MAX_EXPAND_FILE_CHARS`].
/// Runs in `spawn_blocking` and silently skips added, secret, binary, oversized (>2 MB), missing,
/// or path-escaping files.
pub(crate) async fn load_expanded_diffs(
    ws: Arc<RepoWorkspace>,
    files: &[FileDiff],
) -> HashMap<String, String> {
    let candidates: Vec<FileDiff> = files
        .iter()
        .filter(|file| {
            file.status != FileStatus::Added
                && !file.patch.as_deref().unwrap_or("").is_empty()
                && diff_block(file).chars().count() < MAX_EXPAND_FILE_CHARS
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return HashMap::new();
    }
    match tokio::task::spawn_blocking(move || load_expanded_diffs_sync(&ws, &candidates)).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(error = %err, "loading expanded diffs failed");
            HashMap::new()
        }
    }
}

fn load_expanded_diffs_sync(ws: &RepoWorkspace, files: &[FileDiff]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for file in files {
        let key = normalize_path(&file.path);
        let Ok(resolved) = ws.resolve(&key) else {
            continue;
        };
        if !resolved.abs().is_file() {
            continue;
        }
        let Ok(text) = read_text(resolved.abs()) else {
            continue;
        };
        out.insert(key, expand_diff_block(file, &text));
    }
    out
}

/// Runs `call` through the cache. Fresh content is redacted before it is cached, so cached
/// entries hold the redacted text.
fn run_cached(
    ws: &RepoWorkspace,
    cache: &ToolCache,
    call: &ReadOnlyCall,
    limits: SearchLimits,
) -> Result<String, ToolError> {
    let (key, target) = call.normalize(ws)?;
    if let Some(hit) = cache.get(&key) {
        return Ok(hit);
    }
    let mut found = run_uncached(ws, &key, &target, limits)?;
    if let Cow::Owned(redacted) = redact_secrets(&found.text) {
        found.text = redacted;
        cache.redacted.fetch_add(1, Ordering::Relaxed);
    }
    // A cut caused by load is not deterministic: a later identical call gets a fresh try.
    if !found.time_cut {
        cache.insert(key, found.text.clone());
    }
    Ok(found.text)
}

/// Runs one normalized call against the workspace: raw text, no cache.
fn run_uncached(
    ws: &RepoWorkspace,
    key: &CacheKey,
    target: &RepoPath,
    limits: SearchLimits,
) -> Result<ToolText, ToolError> {
    let found = match key {
        CacheKey::ReadFile { start, end, .. } => {
            ToolText::complete(read_file(target, *start, *end)?)
        }
        CacheKey::Grep {
            pattern,
            glob,
            case_sensitive,
            ..
        } => grep(
            ws,
            target,
            pattern,
            glob.as_deref(),
            *case_sensitive,
            limits,
        )?,
        CacheKey::ListDir { .. } => ToolText::complete(list_dir(target)?),
        CacheKey::FindFiles { pattern, .. } => find_files(ws, target, pattern, limits)?,
    };
    Ok(found)
}

/// The text of a read-only call, and whether the search time budget cut it short.
struct ToolText {
    text: String,
    time_cut: bool,
}

impl ToolText {
    fn complete(text: String) -> Self {
        Self {
            text,
            time_cut: false,
        }
    }
}

/// Reads a text file of at most 2 MB without a NUL byte in its first 8 KB. Invalid UTF-8 is
/// replaced, not rejected.
pub(crate) fn read_text(abs: &Path) -> Result<String, ToolError> {
    let file = File::open(abs)?;
    if file.metadata()?.len() > MAX_FILE_BYTES {
        return Err(ToolError::TooLarge);
    }
    let mut bytes = Vec::new();
    // `take` guards against a file that grew after the size check.
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(ToolError::TooLarge);
    }
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
        return Err(ToolError::Binary);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_file(path: &RepoPath, start: u32, end: u32) -> Result<String, ToolError> {
    let rel = path.rel();
    if path.abs().is_dir() {
        return Err(ToolError::IsDirectory(rel.to_owned()));
    }
    if !path.abs().is_file() {
        return Err(ToolError::NotFound(rel.to_owned()));
    }
    let text = read_text(path.abs())?;
    // `lines` also strips the `\r` of CRLF endings.
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(format!("[{rel}: lines 0-0 of 0]"));
    }
    let first = start as usize;
    if first > total {
        return Err(ToolError::PastEnd {
            path: rel.to_owned(),
            start,
            total,
        });
    }
    let last = (end as usize).min(total);
    // The final header is never longer than this one (its end line is at most `last`).
    let header_chars = format!("[{rel}: lines {first}-{last} of {total}]\n")
        .chars()
        .count();
    let mut budget = READ_MAX_CHARS.saturating_sub(header_chars);
    let mut body = String::new();
    let mut shown = first - 1;
    for (n, line) in (first..=last).zip(&lines[first - 1..last]) {
        let prefix = format!("{n}: ");
        let needed = prefix.chars().count() + line.chars().count() + 1;
        if needed > budget {
            if body.is_empty() {
                // A single huge line (minified code): show its start rather than nothing.
                let room = budget.saturating_sub(prefix.chars().count() + LINE_TRUNCATED.len() + 1);
                body.push_str(&prefix);
                body.extend(line.chars().take(room));
                body.push_str(LINE_TRUNCATED);
                body.push('\n');
                shown = n;
            }
            break;
        }
        budget -= needed;
        body.push_str(&prefix);
        body.push_str(line);
        body.push('\n');
        shown = n;
    }
    body.pop();
    Ok(format!("[{rel}: lines {first}-{shown} of {total}]\n{body}"))
}

/// A walk below `dir` that honours `.gitignore` inside the clone only and never yields `.git`
/// or secret paths.
fn walk(ws: &RepoWorkspace, dir: &Path) -> ignore::Walk {
    let root = ws.root().to_path_buf();
    WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(false)
        .git_global(false)
        .ignore(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            relative(&root, entry.path()).is_some_and(|rel| !is_secret_path(&rel))
        })
        .build()
}

/// `path` relative to `root`, `/`-separated.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

/// Walks files under `target` until `deadline` passes or `visit` breaks. Returns `true` when
/// `deadline` cut the walk short.
fn walk_files(
    ws: &RepoWorkspace,
    target: &RepoPath,
    deadline: Instant,
    mut visit: impl FnMut(String, PathBuf) -> ControlFlow<()>,
) -> bool {
    let root = ws.root();
    for entry in walk(ws, target.abs()).filter_map(Result::ok) {
        if Instant::now() >= deadline {
            return true;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(rel) = relative(root, entry.path()) else {
            continue;
        };
        if visit(rel, entry.into_path()).is_break() {
            return false;
        }
    }
    false
}

fn expect_dir(path: &RepoPath) -> Result<(), ToolError> {
    if path.abs().is_file() {
        Err(ToolError::NotADirectory(path.rel().to_owned()))
    } else if !path.abs().is_dir() {
        Err(ToolError::NotFound(path.rel().to_owned()))
    } else {
        Ok(())
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| ToolError::Glob(excerpt(&e.to_string(), MAX_ECHO_CHARS)))
}

/// Output under a character cap that counts what it holds.
struct Capped {
    text: String,
    chars: usize,
    limit: usize,
}

impl Capped {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            limit,
        }
    }

    /// Appends `line` plus a newline, or returns `false` if it would pass the cap.
    fn push_line(&mut self, line: &str) -> bool {
        let len = line.chars().count() + 1;
        if self.chars + len > self.limit {
            return false;
        }
        self.text.push_str(line);
        self.text.push('\n');
        self.chars += len;
        true
    }
}

fn grep(
    ws: &RepoWorkspace,
    target: &RepoPath,
    pattern: &str,
    glob: Option<&str>,
    case_sensitive: bool,
    limits: SearchLimits,
) -> Result<ToolText, ToolError> {
    let deadline = Instant::now() + limits.time_budget;
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| ToolError::Regex(excerpt(&e.to_string(), MAX_ECHO_CHARS)))?;
    let glob = glob.map(compile_glob).transpose()?;
    if !target.abs().exists() {
        return Err(ToolError::NotFound(target.rel().to_owned()));
    }

    let mut search = GrepSearch {
        regex: &regex,
        deadline,
        line_window: limits.line_window,
        out: Capped::new(GREP_MAX_CHARS - NOTE_RESERVE_CHARS),
        groups: 0,
        long_lines: 0,
        scanned: 0,
        skipped: 0,
        truncated: None,
        timed_out: false,
    };
    let walk_timed_out = walk_files(ws, target, deadline, |file_rel, abs| {
        if glob.as_ref().is_some_and(|g| !g.is_match(&file_rel)) {
            return ControlFlow::Continue(());
        }
        if search.scanned == GREP_MAX_FILES {
            search.truncated = Some(format!(
                "stopped after scanning {GREP_MAX_FILES} files; narrow path or glob"
            ));
            return ControlFlow::Break(());
        }
        search.scanned += 1;
        let Ok(text) = read_text(&abs) else {
            search.skipped += 1;
            return ControlFlow::Continue(());
        };
        if let Err(reason) = search.file(&file_rel, &text) {
            search.truncated = Some(reason);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if walk_timed_out {
        search.timed_out = true;
        search.truncated = Some(time_limit_note());
    }

    let mut result = search.out.text;
    if search.groups == 0 && search.truncated.is_none() {
        let place = if target.rel().is_empty() {
            "the repository"
        } else {
            target.rel()
        };
        let pattern = excerpt(pattern, MAX_ECHO_CHARS);
        result = format!("No matches for `{pattern}` in {place}.\n");
    }
    if search.skipped > 0 {
        result.push_str(&format!(
            "[{} file(s) skipped: binary, unreadable or larger than 2 MB]\n",
            search.skipped
        ));
    }
    if search.long_lines > 0 {
        let kib = limits.line_window / 1024;
        result.push_str(&format!(
            "[search truncated: {} line(s) longer than {kib} KiB were only searched in their first {kib} KiB]\n",
            search.long_lines
        ));
    }
    if let Some(reason) = search.truncated {
        result.push_str(&format!("[search truncated: {reason}]\n"));
    }
    result.pop();
    Ok(ToolText {
        text: result,
        time_cut: search.timed_out,
    })
}

fn time_limit_note() -> String {
    "time limit reached; narrow the pattern, path or glob".to_owned()
}

/// State of one `grep` call across files.
struct GrepSearch<'r> {
    regex: &'r Regex,
    deadline: Instant,
    line_window: usize,
    out: Capped,
    groups: usize,
    /// Lines matched only within their first `line_window` bytes.
    long_lines: usize,
    scanned: usize,
    skipped: usize,
    truncated: Option<String>,
    /// The deadline passed before the search finished.
    timed_out: bool,
}

impl GrepSearch<'_> {
    /// Appends the match groups of one file. `Err` carries the truncation reason; matches
    /// found before the deadline are still shown.
    fn file(&mut self, rel: &str, text: &str) -> Result<(), String> {
        let lines: Vec<&str> = text.lines().collect();
        let mut matches = Vec::new();
        let mut timed_out = false;
        for (i, line) in lines.iter().enumerate() {
            if Instant::now() >= self.deadline {
                timed_out = true;
                break;
            }
            let window = if line.len() > self.line_window {
                self.long_lines += 1;
                &line[..line.floor_char_boundary(self.line_window)]
            } else {
                line
            };
            if self.regex.is_match(window) {
                matches.push(i);
            }
        }
        self.render(rel, &lines, &matches)?;
        if timed_out {
            self.timed_out = true;
            return Err(time_limit_note());
        }
        Ok(())
    }

    /// Prints `matches` (sorted line indexes) with their context, merging nearby windows.
    fn render(&mut self, rel: &str, lines: &[&str], matches: &[usize]) -> Result<(), String> {
        let mut windows: Vec<(usize, usize)> = Vec::new();
        for &m in matches {
            let from = m.saturating_sub(GREP_CONTEXT_LINES);
            let to = (m + GREP_CONTEXT_LINES).min(lines.len() - 1);
            match windows.last_mut() {
                Some(last) if from <= last.1 + 1 => last.1 = to,
                _ => windows.push((from, to)),
            }
        }
        let char_limit = || {
            format!("{GREP_MAX_CHARS} character limit reached; narrow the pattern, path or glob")
        };
        for (from, to) in windows {
            if self.groups == GREP_MAX_GROUPS {
                return Err(format!(
                    "{GREP_MAX_GROUPS} match group limit reached; narrow the pattern, path or glob"
                ));
            }
            let separator_ok = self.out.text.is_empty() || self.out.push_line("");
            if !separator_ok || !self.out.push_line(rel) {
                return Err(char_limit());
            }
            self.groups += 1;
            for (i, line) in lines.iter().enumerate().take(to + 1).skip(from) {
                let mark = if matches.binary_search(&i).is_ok() {
                    '>'
                } else {
                    '-'
                };
                let line = excerpt(line, GREP_MAX_LINE_CHARS);
                if !self.out.push_line(&format!("{}{mark} {line}", i + 1)) {
                    return Err(char_limit());
                }
            }
        }
        Ok(())
    }
}

/// `text` cut to `max` characters, marked when cut. Keeps model-supplied text (patterns,
/// tool names, parser messages) and minified lines from flooding the output.
fn excerpt(text: &str, max: usize) -> String {
    truncate_with_marker(text, max, LINE_TRUNCATED).into_owned()
}

fn list_dir(dir: &RepoPath) -> Result<String, ToolError> {
    expect_dir(dir)?;
    let rel = dir.rel();
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir.abs())?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().ok()?.is_dir();
            (!is_secret_path(&name)).then_some((name, is_dir))
        })
        .collect();
    entries.sort();
    let total = entries.len();
    let shown_dir = if rel.is_empty() { "." } else { rel };
    let noun = if total == 1 { "entry" } else { "entries" };
    let mut out = Capped::new(LIST_MAX_CHARS - NOTE_RESERVE_CHARS);
    out.push_line(&format!("[{shown_dir}: {total} {noun}]"));
    let mut shown = 0;
    for (name, is_dir) in entries.iter().take(LIST_MAX_ENTRIES) {
        let line = if *is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        if !out.push_line(&line) {
            break;
        }
        shown += 1;
    }
    let mut result = out.text;
    if shown < total {
        result.push_str(&format!(
            "[listing truncated: showing {shown} of {total} entries]\n"
        ));
    }
    result.pop();
    Ok(result)
}

fn find_files(
    ws: &RepoWorkspace,
    dir: &RepoPath,
    pattern: &str,
    limits: SearchLimits,
) -> Result<ToolText, ToolError> {
    let deadline = Instant::now() + limits.time_budget;
    let glob = compile_glob(pattern)?;
    expect_dir(dir)?;
    let mut found: Vec<String> = Vec::new();
    let timed_out = walk_files(ws, dir, deadline, |rel, _| {
        if glob.is_match(&rel) {
            found.push(rel);
            // One extra match tells whether the list was cut.
            if found.len() > LIST_MAX_ENTRIES {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });
    let more = found.len() > LIST_MAX_ENTRIES;
    found.truncate(LIST_MAX_ENTRIES);
    found.sort();
    let mut out = Capped::new(LIST_MAX_CHARS - NOTE_RESERVE_CHARS);
    if found.is_empty() {
        let pattern = excerpt(pattern, MAX_ECHO_CHARS);
        out.push_line(&format!("No files match `{pattern}`."));
    }
    let mut shown = 0;
    for path in &found {
        if !out.push_line(path) {
            break;
        }
        shown += 1;
    }
    let mut result = out.text;
    if more || shown < found.len() {
        result.push_str(&format!(
            "[results truncated: showing {shown} matches; narrow the pattern or path]\n"
        ));
    }
    if timed_out {
        result.push_str("[results truncated: time limit reached; narrow the pattern or path]\n");
    }
    result.pop();
    Ok(ToolText {
        text: result,
        time_cut: timed_out,
    })
}

fn prop(ty: &str, description: &str) -> Value {
    json!({"type": ty, "description": description})
}

fn tool_def(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDef {
    let mut parameters = json!({
        "type": "object",
        "properties": properties,
    });
    if !required.is_empty()
        && let Some(obj) = parameters.as_object_mut()
    {
        obj.insert("required".into(), json!(required));
    }
    ToolDef {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

fn read_only_defs() -> Vec<ToolDef> {
    vec![
        tool_def(
            "read_file",
            "Read a text file from the pull request's head checkout. Returns at most \
             200 lines and 8,000 characters, starting with a header \
             `[<path>: lines <start>-<end> of <total>]`, then one `<line number>: <text>` row \
             per line. When <end> is below <total>, call again with start_line = <end> + 1. \
             Refuses binary files, files over 2 MB (use grep instead), secret files and .git.",
            json!({
                "path": prop("string", "File path relative to the repository root, with / separators, e.g. src/main.rs"),
                "start_line": prop("integer", "First line to return, 1-based. Defaults to 1."),
                "end_line": prop("integer", "Last line to return, inclusive. Defaults to start_line + 199.")
            }),
            &["path"],
        ),
        tool_def(
            "grep",
            "Search file contents with a regular expression (Rust regex syntax, \
             matched per line). Skips .gitignore'd, binary, secret and .git files and files \
             over 2 MB. Output: groups separated by a blank line; each group starts with the \
             file path, then lines `<n>> <text>` for matches and `<n>- <text>` for up to 2 \
             context lines around them. Stops at 40 groups, 8,000 characters, 20,000 files \
             scanned or 10 seconds and then ends with `[search truncated: ...]`. Only the \
             first 64 KiB of each line is searched; printed lines longer than 500 characters \
             are cut.",
            json!({
                "pattern": prop("string", "Regular expression to search for."),
                "path": prop("string", "Directory or file to search, relative to the repository root. Defaults to the whole repository."),
                "glob": prop("string", "Only search files whose path relative to the repository root matches this glob; * also matches /, so *.rs finds Rust files at any depth."),
                "case_sensitive": prop("boolean", "Match case exactly. Defaults to true.")
            }),
            &["pattern"],
        ),
        tool_def(
            "list_dir",
            "List the immediate entries of a directory. Output: a header \
             `[<dir>: <count> entries]`, then one name per line sorted alphabetically, with \
             a trailing / for directories. Secret files and .git are hidden. Shows at most \
             150 entries and 4,000 characters, then `[listing truncated: ...]`.",
            json!({
                "path": prop("string", "Directory relative to the repository root. Defaults to the root.")
            }),
            &[],
        ),
        tool_def(
            "find_files",
            "Find files whose path relative to the repository root matches a glob. \
             * also matches /, so *.rs finds Rust files at any depth; use **/name.ext for a \
             file name in any directory. Skips .gitignore'd, secret and .git files. Output: \
             one path per line, sorted, at most 150 paths and 4,000 characters; stops after \
             10 seconds. A cut list ends with `[results truncated: ...]`.",
            json!({
                "pattern": prop("string", "Glob matched against paths relative to the repository root, e.g. **/Cargo.toml or src/**/*.ts"),
                "path": prop("string", "Only search below this directory, relative to the repository root. Defaults to the root.")
            }),
            &["pattern"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::*;

    fn workspace(files: &[(&str, &[u8])]) -> Arc<RepoWorkspace> {
        Arc::new(RepoWorkspace::from_files(files))
    }

    fn toolbox(files: &[(&str, &[u8])]) -> Toolbox {
        Toolbox::new(workspace(files))
    }

    async fn ok(tb: &Toolbox, name: &str, args: Value) -> String {
        let out = tb.execute_read_only(name, &args).await;
        assert!(!out.is_error, "{name} {args}: {}", out.content);
        out.content
    }

    async fn err(tb: &Toolbox, name: &str, args: Value) -> String {
        let out = tb.execute_read_only(name, &args).await;
        assert!(out.is_error, "{name} {args} should fail: {}", out.content);
        assert!(out.content.starts_with("Error: "), "{}", out.content);
        out.content
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn headers(content: &str, path: &str) -> usize {
        content.lines().filter(|l| *l == path).count()
    }

    #[tokio::test]
    async fn read_file_windows_line_range() {
        let body = numbered(10);
        let tb = toolbox(&[("src/a.rs", body.as_bytes())]);
        let out = ok(
            &tb,
            "read_file",
            json!({"path": "src/a.rs", "start_line": 3, "end_line": 5}),
        )
        .await;
        assert_eq!(
            out,
            "[src/a.rs: lines 3-5 of 10]\n3: line 3\n4: line 4\n5: line 5"
        );
        let out = ok(
            &tb,
            "read_file",
            json!({"path": "src/a.rs", "start_line": 8, "end_line": 50}),
        )
        .await;
        assert_eq!(
            out,
            "[src/a.rs: lines 8-10 of 10]\n8: line 8\n9: line 9\n10: line 10"
        );
        let out = ok(&tb, "read_file", json!({"path": "src/a.rs"})).await;
        assert!(
            out.starts_with("[src/a.rs: lines 1-10 of 10]\n1: line 1\n"),
            "{out}"
        );
        assert!(out.ends_with("10: line 10"), "{out}");
    }

    #[tokio::test]
    async fn read_file_normalizes_crlf_and_handles_empty_files() {
        let tb = toolbox(&[("crlf.txt", b"a\r\nb\r\n"), ("empty.txt", b"")]);
        assert_eq!(
            ok(&tb, "read_file", json!({"path": "crlf.txt"})).await,
            "[crlf.txt: lines 1-2 of 2]\n1: a\n2: b"
        );
        assert_eq!(
            ok(&tb, "read_file", json!({"path": "empty.txt"})).await,
            "[empty.txt: lines 0-0 of 0]"
        );
    }

    #[tokio::test]
    async fn read_file_caps_at_200_lines() {
        let body = numbered(300);
        let tb = toolbox(&[("big.txt", body.as_bytes())]);
        let out = ok(&tb, "read_file", json!({"path": "big.txt"})).await;
        assert!(out.starts_with("[big.txt: lines 1-200 of 300]\n"), "{out}");
        assert_eq!(out.lines().count(), 201);
        assert!(out.ends_with("200: line 200"));
        let out = ok(
            &tb,
            "read_file",
            json!({"path": "big.txt", "start_line": 250}),
        )
        .await;
        assert!(
            out.starts_with("[big.txt: lines 250-300 of 300]\n"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn read_file_caps_at_8000_chars() {
        let line = "a".repeat(199);
        let body: String = (0..100).map(|_| format!("{line}\n")).collect();
        let long_line = "b".repeat(20_000);
        let tb = toolbox(&[
            ("wide.txt", body.as_bytes()),
            ("min.js", long_line.as_bytes()),
        ]);

        let out = ok(&tb, "read_file", json!({"path": "wide.txt"})).await;
        assert!(out.chars().count() <= 8_000);
        let header = out.lines().next().unwrap();
        let shown: usize = header
            .trim_start_matches("[wide.txt: lines 1-")
            .trim_end_matches(" of 100]")
            .parse()
            .unwrap();
        assert!(shown > 30 && shown < 100, "{header}");
        assert_eq!(out.lines().count(), shown + 1);
        assert!(out.ends_with(&format!("{shown}: {line}")));

        let out = ok(&tb, "read_file", json!({"path": "min.js"})).await;
        assert!(out.chars().count() <= 8_000);
        assert!(
            out.starts_with("[min.js: lines 1-1 of 1]\n1: bbb"),
            "{}",
            &out[..40]
        );
        assert!(out.ends_with("[line truncated]"));
    }

    #[tokio::test]
    async fn read_file_refuses_binary_large_secret_and_escaping_paths() {
        let large = vec![b'a'; 2 * 1024 * 1024 + 1];
        let tb = toolbox(&[
            ("bin.dat", b"abc\0def"),
            ("large.txt", &large),
            (".env", b"TOKEN=x"),
            ("config/secrets.yml", b"x"),
            (".git/HEAD", b"ref"),
            ("src/a.rs", b"fn a() {}\n"),
        ]);
        assert!(
            err(&tb, "read_file", json!({"path": "bin.dat"}))
                .await
                .contains("binary")
        );
        assert!(
            err(&tb, "read_file", json!({"path": "large.txt"}))
                .await
                .contains("file exceeds 2 MB limit; use grep instead")
        );
        for path in [
            ".env",
            "config/secrets.yml",
            ".git/HEAD",
            "../outside.txt",
            "/etc/passwd",
            "",
        ] {
            err(&tb, "read_file", json!({"path": path})).await;
        }
        assert!(
            err(&tb, "read_file", json!({"path": "nope.rs"}))
                .await
                .contains("not found")
        );
        assert!(
            err(&tb, "read_file", json!({"path": "src"}))
                .await
                .contains("directory")
        );
        assert!(
            err(
                &tb,
                "read_file",
                json!({"path": "src/a.rs", "start_line": 5})
            )
            .await
            .contains("past the end")
        );
        err(
            &tb,
            "read_file",
            json!({"path": "src/a.rs", "start_line": 3, "end_line": 2}),
        )
        .await;
    }

    #[tokio::test]
    async fn bad_tool_names_and_arguments_are_errors() {
        let tb = toolbox(&[("a.rs", b"x\n")]);
        assert!(
            err(&tb, "write_file", json!({}))
                .await
                .contains("unknown tool")
        );
        assert!(
            err(&tb, "read_file", json!({"path": 5}))
                .await
                .contains("invalid type")
        );
        assert!(
            err(&tb, "read_file", json!({}))
                .await
                .contains("missing field `path`")
        );
        assert!(
            err(&tb, "grep", json!("needle"))
                .await
                .contains("invalid type")
        );
        let raw = json!({"__raw_parse_error": "EOF while parsing an object", "raw": "{\"path\":"});
        for name in ["read_file", "grep", "list_dir", "find_files"] {
            assert!(
                err(&tb, name, raw.clone())
                    .await
                    .contains("EOF while parsing"),
                "{name}"
            );
        }
        // Providers send `null` or `{}` for a call without arguments.
        assert!(ok(&tb, "list_dir", Value::Null).await.contains("a.rs"));
        assert!(ok(&tb, "list_dir", json!({})).await.contains("a.rs"));
    }

    #[tokio::test]
    async fn grep_shows_two_context_lines_and_merges_close_matches() {
        let mut lines: Vec<String> = (1..=20).map(|i| format!("l{i}")).collect();
        lines[4] = "needle five".into();
        let one = lines.join("\n");
        lines[8] = "needle nine".into();
        lines[17] = "needle eighteen".into();
        let three = lines.join("\n");
        let tb = toolbox(&[("src/a.rs", one.as_bytes()), ("src/b.rs", three.as_bytes())]);

        let out = ok(
            &tb,
            "grep",
            json!({"pattern": "needle", "path": "src/a.rs"}),
        )
        .await;
        assert_eq!(out, "src/a.rs\n3- l3\n4- l4\n5> needle five\n6- l6\n7- l7");

        let out = ok(
            &tb,
            "grep",
            json!({"pattern": "needle", "path": "src/b.rs"}),
        )
        .await;
        assert_eq!(headers(&out, "src/b.rs"), 2, "{out}");
        assert!(
            out.contains("5> needle five\n6- l6\n7- l7\n8- l8\n9> needle nine\n10- l10\n11- l11\n")
        );
        assert!(
            out.ends_with("16- l16\n17- l17\n18> needle eighteen\n19- l19\n20- l20"),
            "{out}"
        );
        assert!(!out.contains("12- "));
    }

    #[tokio::test]
    async fn grep_filters_by_path_and_glob_and_case() {
        let tb = toolbox(&[
            ("src/a.rs", b"needle\n"),
            ("src/b.txt", b"needle\n"),
            ("docs/c.rs", b"Needle\n"),
        ]);
        let out = ok(&tb, "grep", json!({"pattern": "needle", "path": "src"})).await;
        assert_eq!(
            headers(&out, "src/a.rs") + headers(&out, "src/b.txt"),
            2,
            "{out}"
        );
        assert!(!out.contains("docs/c.rs"));

        let out = ok(
            &tb,
            "grep",
            json!({"pattern": "(?i)needle", "glob": "*.rs"}),
        )
        .await;
        assert!(
            out.contains("src/a.rs") && out.contains("docs/c.rs"),
            "{out}"
        );
        assert!(!out.contains("src/b.txt"));

        let out = ok(&tb, "grep", json!({"pattern": "NEEDLE", "path": "docs"})).await;
        assert!(out.starts_with("No matches"), "{out}");
        let out = ok(
            &tb,
            "grep",
            json!({"pattern": "NEEDLE", "path": "docs", "case_sensitive": false}),
        )
        .await;
        assert_eq!(out, "docs/c.rs\n1> Needle");

        err(&tb, "grep", json!({"pattern": "needle", "path": "missing"})).await;
        err(&tb, "grep", json!({"pattern": "needle", "glob": "["})).await;
    }

    #[tokio::test]
    async fn grep_skips_gitignored_git_secret_and_binary_files() {
        let tb = toolbox(&[
            (".gitignore", b"build/\n"),
            ("build/out.rs", b"needle\n"),
            (".git/config", b"needle\n"),
            (".env", b"needle\n"),
            ("keys/server.pem", b"needle\n"),
            ("bin.dat", b"needle\0\n"),
            (".github/ci.yml", b"needle\n"),
            ("src/a.rs", b"needle\n"),
        ]);
        let out = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        assert!(out.contains(".github/ci.yml\n1> needle"), "{out}");
        assert!(out.contains("src/a.rs\n1> needle"), "{out}");
        for hidden in ["build/", ".git/", ".env", "server.pem", "bin.dat"] {
            assert!(!out.contains(hidden), "{hidden} leaked: {out}");
        }
        let out = ok(&tb, "grep", json!({"pattern": "needle", "glob": ".env"})).await;
        assert!(out.starts_with("No matches"), "{out}");
        err(&tb, "grep", json!({"pattern": "needle", "path": ".env"})).await;
        err(&tb, "grep", json!({"pattern": "needle", "path": ".git"})).await;
    }

    #[tokio::test]
    async fn grep_caps_groups_and_chars() {
        let names: Vec<String> = (0..50).map(|i| format!("f{i:02}.rs")).collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| (n.as_str(), &b"needle\n"[..]))
            .collect();
        let tb = toolbox(&files);
        let out = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        let groups = out
            .lines()
            .filter(|l| l.starts_with('f') && l.ends_with(".rs"))
            .count();
        assert_eq!(groups, 40, "{out}");
        assert!(out.contains("f39.rs") && !out.contains("f40.rs"));
        assert!(
            out.ends_with(']') && out.contains("[search truncated: "),
            "{out}"
        );

        let wide = format!("{}\n", "needle ".repeat(60)).repeat(30);
        let tb = toolbox(&[("a.txt", wide.as_bytes()), ("b.txt", wide.as_bytes())]);
        let out = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        assert!(out.chars().count() <= 8_000);
        assert!(out.contains("[search truncated: "), "{out}");
    }

    #[tokio::test]
    async fn grep_rejects_bad_empty_and_oversized_regexes() {
        let tb = toolbox(&[("a.rs", b"x\n")]);
        assert!(
            err(&tb, "grep", json!({"pattern": "("}))
                .await
                .contains("regex")
        );
        err(&tb, "grep", json!({"pattern": ""})).await;
        assert!(
            err(&tb, "grep", json!({"pattern": "\\w{1000}{1000}"}))
                .await
                .contains("size limit")
        );
    }

    fn limited(files: &[(&str, &[u8])], time_budget: Duration, line_window: usize) -> Toolbox {
        let mut tb = toolbox(files);
        tb.limits = SearchLimits {
            time_budget,
            line_window,
        };
        tb
    }

    #[tokio::test]
    async fn search_time_budget_cuts_calls_without_caching_them() {
        let files: &[(&str, &[u8])] = &[("src/a.rs", b"needle\n")];
        let cut = limited(files, Duration::ZERO, GREP_LINE_WINDOW);
        assert_eq!(
            ok(&cut, "grep", json!({"pattern": "needle"})).await,
            "[search truncated: time limit reached; narrow the pattern, path or glob]"
        );
        assert_eq!(
            ok(&cut, "find_files", json!({"pattern": "*.rs"})).await,
            "No files match `*.rs`.\n\
             [results truncated: time limit reached; narrow the pattern or path]"
        );
        ok(&cut, "grep", json!({"pattern": "needle"})).await;
        ok(&cut, "find_files", json!({"pattern": "*.rs"})).await;
        assert_eq!(cut.cache.hits(), 0, "a time-cut result is recomputed");

        let full = toolbox(files);
        for _ in 0..2 {
            ok(&full, "grep", json!({"pattern": "needle"})).await;
            ok(&full, "find_files", json!({"pattern": "*.rs"})).await;
        }
        assert_eq!(full.cache.hits(), 2, "a finished search is cached");
    }

    #[tokio::test]
    async fn grep_bounds_slow_regexes_by_the_time_budget() {
        // Pseudo-random a/b lines keep this pattern off the fast regex engines.
        let mut state = 12_345_u32;
        let mut text = String::new();
        for _ in 0..400 {
            for _ in 0..4_000 {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                text.push(if (state >> 16) & 1 == 0 { 'a' } else { 'b' });
            }
            text.push('\n');
        }
        let tb = limited(
            &[("ab.txt", text.as_bytes())],
            Duration::from_millis(50),
            GREP_LINE_WINDOW,
        );
        let started = Instant::now();
        let out = ok(&tb, "grep", json!({"pattern": "a[ab]{300}[c-y]"})).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(
            out.ends_with(
                "[search truncated: time limit reached; narrow the pattern, path or glob]"
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn grep_matches_only_the_line_window_of_long_lines() {
        // Line 3 is cut inside a two-byte character, which must not panic.
        let text = format!(
            "{}needle\nneedle\na{}\n",
            "x".repeat(1_030),
            "\u{e9}".repeat(600)
        );
        let tb = limited(&[("a.txt", text.as_bytes())], SEARCH_TIME_BUDGET, 1_024);
        let out = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        assert!(out.starts_with("a.txt\n1- xxx"), "{out}");
        assert!(out.contains("\n2> needle\n3- a\u{e9}"), "{out}");
        assert!(!out.contains("1> "), "{out}");
        assert!(
            out.ends_with(
                "[search truncated: 2 line(s) longer than 1 KiB were only searched in their first 1 KiB]"
            ),
            "{out}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn walks_and_reads_do_not_follow_symlinks_out_of_the_workspace() {
        let outside = tempfile::TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        let tb = toolbox(&[("src/a.rs", b"x\n")]);
        let root = tb.workspace.root();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("file-link"))
            .unwrap();
        let out = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        assert!(out.starts_with("No matches"), "{out}");
        assert_eq!(
            ok(&tb, "find_files", json!({"pattern": "**"})).await,
            "src/a.rs"
        );
        err(&tb, "read_file", json!({"path": "file-link"})).await;
        err(&tb, "grep", json!({"pattern": "needle", "path": "link"})).await;
        err(&tb, "list_dir", json!({"path": "link"})).await;
    }

    #[tokio::test]
    async fn list_dir_hides_git_and_secrets_and_sorts() {
        let tb = toolbox(&[
            ("b.rs", b"x"),
            ("a.rs", b"x"),
            ("src/x.rs", b"x"),
            (".env", b"x"),
            (".env.example", b"x"),
            ("key.pem", b"x"),
            (".git/HEAD", b"x"),
            (".github/ci.yml", b"x"),
        ]);
        assert_eq!(
            ok(&tb, "list_dir", json!({})).await,
            "[.: 5 entries]\n.env.example\n.github/\na.rs\nb.rs\nsrc/"
        );
        assert_eq!(
            ok(&tb, "list_dir", json!({"path": "src/"})).await,
            "[src: 1 entry]\nx.rs"
        );
        err(&tb, "list_dir", json!({"path": ".git"})).await;
        err(&tb, "list_dir", json!({"path": "../"})).await;
        err(&tb, "list_dir", json!({"path": "/"})).await;
        assert!(tb.workspace.resolve_or_root("///").is_err());
        assert!(
            err(&tb, "list_dir", json!({"path": "a.rs"}))
                .await
                .contains("not a directory")
        );
        err(&tb, "list_dir", json!({"path": "missing"})).await;
    }

    #[tokio::test]
    async fn list_dir_and_find_files_cap_at_150_entries() {
        let names: Vec<String> = (0..200).map(|i| format!("d/f{i:03}.rs")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let tb = toolbox(&files);

        let out = ok(&tb, "list_dir", json!({"path": "d"})).await;
        assert!(out.starts_with("[d: 200 entries]\nf000.rs\n"), "{out}");
        assert!(out.contains("f149.rs\n") && !out.contains("f150.rs"));
        assert!(
            out.ends_with("[listing truncated: showing 150 of 200 entries]"),
            "{out}"
        );
        assert!(out.chars().count() <= 4_000);

        let out = ok(&tb, "find_files", json!({"pattern": "*.rs"})).await;
        assert!(out.starts_with("d/f000.rs\n"), "{out}");
        assert_eq!(out.lines().filter(|l| l.starts_with("d/")).count(), 150);
        assert!(out.contains("[results truncated: "), "{out}");
        assert!(out.chars().count() <= 4_000);
    }

    #[tokio::test]
    async fn find_files_matches_relative_paths_and_hides_ignored_git_and_secrets() {
        let tb = toolbox(&[
            (".gitignore", b"build/\n"),
            ("src/a.rs", b"x"),
            ("src/deep/b.rs", b"x"),
            ("c.rs", b"x"),
            ("d.txt", b"x"),
            (".git/x.rs", b"x"),
            ("keys/id_rsa.rs", b"x"),
            ("build/e.rs", b"x"),
        ]);
        assert_eq!(
            ok(&tb, "find_files", json!({"pattern": "*.rs"})).await,
            "c.rs\nsrc/a.rs\nsrc/deep/b.rs"
        );
        assert_eq!(
            ok(&tb, "find_files", json!({"pattern": "**/b.rs"})).await,
            "src/deep/b.rs"
        );
        assert_eq!(
            ok(&tb, "find_files", json!({"pattern": "*.rs", "path": "src"})).await,
            "src/a.rs\nsrc/deep/b.rs"
        );
        assert!(
            ok(&tb, "find_files", json!({"pattern": "*.go"}))
                .await
                .starts_with("No files match")
        );
        err(&tb, "find_files", json!({"pattern": "["})).await;
        err(&tb, "find_files", json!({"pattern": "*", "path": ".git"})).await;
    }

    #[tokio::test]
    async fn cache_answers_repeated_normalized_calls_and_skips_errors() {
        let tb = toolbox(&[("src/a.rs", b"old needle\n")]);
        let file = tb.workspace.root().join("src/a.rs");
        let first = ok(&tb, "read_file", json!({"path": "src/a.rs"})).await;
        let grep = ok(&tb, "grep", json!({"pattern": "needle"})).await;
        fs::write(&file, "new needle\n").unwrap();

        assert_eq!(
            ok(
                &tb,
                "read_file",
                json!({"path": " ./src/a.rs", "start_line": 1})
            )
            .await,
            first
        );
        assert_eq!(
            ok(
                &tb,
                "grep",
                json!({"pattern": "needle", "path": ".", "case_sensitive": true})
            )
            .await,
            grep
        );
        assert_eq!(tb.cache.hits(), 2);
        // A different window is a different key.
        let out = ok(&tb, "read_file", json!({"path": "src/a.rs", "end_line": 1})).await;
        assert!(out.contains("new needle"), "{out}");

        err(&tb, "read_file", json!({"path": "src/b.rs"})).await;
        fs::write(tb.workspace.root().join("src/b.rs"), "b\n").unwrap();
        assert!(
            ok(&tb, "read_file", json!({"path": "src/b.rs"}))
                .await
                .contains("1: b")
        );
        assert_eq!(tb.cache.hits(), 2);
    }

    #[test]
    fn snippet_returns_numbered_window_or_none() {
        let body = numbered(20);
        let ws = workspace(&[
            ("src/a.rs", body.as_bytes()),
            (".env", b"x"),
            ("bin", b"\0"),
        ]);
        assert_eq!(
            read_file_snippet_sync(&ws, "src/a.rs", 10, 2).as_deref(),
            Some(
                "[src/a.rs: lines 8-12 of 20]\n8: line 8\n9: line 9\n10: line 10\n11: line 11\n12: line 12"
            )
        );
        let top = read_file_snippet_sync(&ws, "src/a.rs", 1, 3).unwrap();
        assert!(
            top.starts_with("[src/a.rs: lines 1-4 of 20]\n1: line 1"),
            "{top}"
        );
        for (path, line) in [
            (".env", 1),
            ("bin", 1),
            ("missing.rs", 1),
            ("src/a.rs", 50),
            ("../x", 1),
        ] {
            assert_eq!(read_file_snippet_sync(&ws, path, line, 2), None, "{path}");
        }
    }

    /// Obviously fake credentials in the provider formats.
    const FAKE_AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const FAKE_GITHUB_TOKEN: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[tokio::test]
    async fn tools_redact_and_cache_secrets_while_verifier_snippet_stays_raw() {
        let body = format!(
            "const REGION = 1;\nconst KEY = \"{FAKE_AWS_KEY}\";\nlet token = \"{FAKE_GITHUB_TOKEN}\";\n"
        );
        let tb = toolbox(&[("src/config.ts", body.as_bytes())]);

        let out = ok(&tb, "read_file", json!({"path": "src/config.ts"})).await;
        assert_eq!(
            out,
            "[src/config.ts: lines 1-3 of 3]\n1: const REGION = 1;\n2: const KEY = \"[REDACTED]\";\n3: let token = \"[REDACTED]\";"
        );
        assert_eq!(tb.redacted_outputs(), 1);

        // Cached entry is already redacted and a normalized repeat hits cache without recounting.
        let entries: Vec<String> = tb.cache.entries.read().unwrap().values().cloned().collect();
        assert_eq!(entries, vec![out.clone()]);
        assert_eq!(
            ok(&tb, "read_file", json!({"path": "./src/config.ts"})).await,
            out
        );
        assert_eq!(tb.cache.hits(), 1);
        assert_eq!(tb.redacted_outputs(), 1);

        // grep masks secrets too.
        let grep_out = ok(&tb, "grep", json!({"pattern": "token"})).await;
        assert!(!grep_out.contains(FAKE_GITHUB_TOKEN), "{grep_out}");
        assert!(
            grep_out.contains("3> let token = \"[REDACTED]\";"),
            "{grep_out}"
        );
        assert_eq!(tb.redacted_outputs(), 2);

        // Verifier snippet bypasses cache and stays unredacted.
        let snippet = tb.read_file_snippet("src/config.ts", 2, 1).await.unwrap();
        assert_eq!(
            snippet,
            format!(
                "[src/config.ts: lines 1-3 of 3]\n1: const REGION = 1;\n2: const KEY = \"{FAKE_AWS_KEY}\";\n3: let token = \"{FAKE_GITHUB_TOKEN}\";"
            )
        );
        assert_eq!(tb.cache.hits(), 1);
    }

    fn assert_portable_schema(schema: &Value) {
        const ALLOWED: &[&str] = &[
            "type",
            "properties",
            "required",
            "description",
            "items",
            "enum",
        ];
        let map = schema.as_object().unwrap();
        for (key, value) in map {
            assert!(ALLOWED.contains(&key.as_str()), "schema key {key}");
            if key == "properties" {
                for prop in value.as_object().unwrap().values() {
                    assert_portable_schema(prop);
                }
            }
            if key == "items" {
                assert_portable_schema(value);
            }
        }
    }

    #[test]
    fn tool_defs_are_named_and_portable() {
        let tb = toolbox(&[]);
        let names: Vec<&str> = tb
            .read_only_tool_defs()
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["read_file", "grep", "list_dir", "find_files"]);
        for def in tb.read_only_tool_defs() {
            assert_eq!(def.parameters["type"], "object", "{}", def.name);
            assert!(def.parameters["properties"].is_object());
            // Omitted or non-empty: an empty `required` is not known to be accepted by Gemini.
            if let Some(required) = def.parameters.get("required") {
                let names = required.as_array().unwrap();
                assert!(!names.is_empty(), "{} has an empty required", def.name);
            }
            assert!(!def.description.is_empty());
            assert_portable_schema(&def.parameters);
        }
    }

    #[tokio::test]
    async fn load_expanded_diffs_skips_added_oversized_and_secret_files() {
        use adus_git::{FileDiff, FileStatus};

        let big_patch = format!("@@ -1,1 +1,2 @@\n a\n+{}\n", "x".repeat(20_500));
        let a_body: String = (1..=30).map(|i| format!("line_{i}\n")).collect();
        let ws = workspace(&[
            ("src/a.rs", a_body.as_bytes()),
            ("src/new.rs", b"fn new_fn() {}\n"),
            (".env", b"SECRET=12345678\n"),
            ("src/big.rs", b"a\n"),
        ]);
        let files = vec![
            FileDiff {
                path: "src/a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                patch: Some("@@ -20,1 +20,2 @@\n line_20\n+added\n".into()),
            },
            FileDiff {
                path: "src/new.rs".into(),
                old_path: None,
                status: FileStatus::Added,
                patch: Some("@@ -0,0 +1,1 @@\n+fn new_fn() {}\n".into()),
            },
            FileDiff {
                path: ".env".into(),
                old_path: None,
                status: FileStatus::Modified,
                patch: Some("@@ -1,1 +1,1 @@\n-OLD=1\n+NEW=2\n".into()),
            },
            FileDiff {
                path: "src/big.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                patch: Some(big_patch),
            },
        ];

        let expanded = load_expanded_diffs(ws, &files).await;
        assert!(expanded.contains_key("src/a.rs"));
        assert!(expanded["src/a.rs"].contains("    5 : line_5"));
        assert!(!expanded.contains_key("src/new.rs"));
        assert!(!expanded.contains_key(".env"));
        assert!(!expanded.contains_key("src/big.rs"));
    }
}
