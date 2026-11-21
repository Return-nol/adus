//! Ephemeral, hardened shallow clone of a PR head for the agentic reviewers.
//!
//! The worker holds decrypted PATs and server secrets, so the clone is built defensively:
//! - the PAT only travels in the `git fetch` environment (`GIT_CONFIG_*`), never in argv, the
//!   URL, `.git/config`, logs or errors (stderr is scrubbed);
//! - every git process runs with a cleared environment, no system/global config, hooks pointed
//!   at `/dev/null`, symlinks checked out as plain files, no fsmonitor, no LFS filters, no
//!   submodules, no redirects and no credential helper;
//! - the clone lives in a [`TempDir`] owned by [`RepoWorkspace`], so it is deleted on drop;
//! - paths handed to tools are [`RepoPath`]s, which only [`RepoWorkspace`] can create after
//!   rejecting traversal, `.git`, secret files and symlinks that leave the root.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use adus_git::CloneSpec;
use async_trait::async_trait;
use base64::Engine;
use tempfile::TempDir;
use tokio::process::Command;

/// Budget for a whole clone (init, fetch, checkout, size check).
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest checkout (excluding `.git`) a review may work on.
pub const MAX_WORKSPACE_BYTES: u64 = 500 * 1024 * 1024;
/// Orphaned workspaces older than this are swept at startup. A clone lives ~15 min at most.
const ORPHAN_MAX_AGE: Duration = Duration::from_secs(3600);
/// Oldest git with `GIT_CONFIG_COUNT` support.
const MIN_GIT_VERSION: (u32, u32) = (2, 31);
/// Accepted by Git for Windows too (`hooks_in_workspace_do_not_fire_during_checkout`).
const NULL_DEVICE: &str = "/dev/null";
/// A stalled HTTP transfer aborts on its own this long after our timeout, so a remote helper
/// that outlives a killed `git` (and still holds the PAT in its env) exits soon after.
const LOW_SPEED_GRACE_SECS: u64 = 5;
const REMOVE_ATTEMPTS: u32 = 20;
const REMOVE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Longest stderr excerpt kept in an error.
const MAX_ERROR_CHARS: usize = 300;

/// Environment variables passed through to git; everything else is cleared.
#[cfg(windows)]
const PASSTHROUGH_ENV: &[&str] = &["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"];
#[cfg(not(windows))]
const PASSTHROUGH_ENV: &[&str] = &["PATH"];

/// Case-insensitive git stderr phrases that mean the credentials were rejected. Never bare
/// status digits: a ref or path may contain "401".
const AUTH_FAILURE_PHRASES: &[&str] = &[
    "authentication failed",
    "invalid username or password",
    "could not read username",
    "terminal prompts disabled",
    "returned error: 401",
    "returned error: 403",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// The provider rejected the PAT.
    #[error("git authentication failed")]
    Unauthorized,
    #[error("repository checkout is larger than the workspace limit")]
    TooLarge,
    #[error("git clone timed out")]
    TimedOut,
    /// Anything else (network, missing ref, old git). The text never contains secrets.
    #[error("git clone unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("invalid path: {0}")]
    Invalid(String),
    #[error("path escapes the repository root")]
    EscapesRoot,
    #[error("paths inside .git are not readable")]
    ForbiddenGitPath,
    #[error("secret files are not readable")]
    SecretFile,
}

/// A path validated to stay inside a [`RepoWorkspace`]. Only the workspace creates one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPath {
    abs: PathBuf,
    rel: String,
}

impl RepoPath {
    /// Absolute path under [`RepoWorkspace::root`].
    pub fn abs(&self) -> &Path {
        &self.abs
    }

    /// Normalized relative path with `/` separators (`""` for the root).
    pub fn rel(&self) -> &str {
        &self.rel
    }
}

/// A checkout on disk. Dropping it deletes the directory.
#[derive(Debug)]
pub struct RepoWorkspace {
    _dir: TempDir,
    canonical_root: PathBuf,
}

impl RepoWorkspace {
    /// Uses `dir` itself as the repository root. For tests and fake cloners.
    pub fn from_temp_dir(dir: TempDir) -> std::io::Result<Self> {
        let root = dir.path().to_path_buf();
        Self::with_root(dir, &root)
    }

    /// `root` must be `dir` or a directory inside it.
    fn with_root(dir: TempDir, root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            canonical_root: std::fs::canonicalize(root)?,
            _dir: dir,
        })
    }

    /// Canonical repository root (on Windows a `\\?\` path). Contains `.git`.
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// Validates `rel` (relative, `/`-separated). The target does not need to exist.
    pub fn resolve(&self, rel: &str) -> Result<RepoPath, PathError> {
        let parts = split_rel(rel)?;
        if parts.is_empty() {
            return Err(PathError::Invalid("empty path".into()));
        }
        self.resolve_parts(&parts)
    }

    /// Like [`Self::resolve`], and the target must be an existing file.
    pub fn resolve_existing_file(&self, rel: &str) -> Result<RepoPath, PathError> {
        let path = self.resolve(rel)?;
        if path.abs.is_file() {
            Ok(path)
        } else {
            Err(PathError::Invalid(format!("not a file: {}", path.rel)))
        }
    }

    /// Like [`Self::resolve`], and the target must be an existing directory. `""` and `"."` are
    /// the root.
    pub fn resolve_dir(&self, rel: &str) -> Result<RepoPath, PathError> {
        let parts = split_rel(rel)?;
        let path = if parts.is_empty() {
            RepoPath {
                abs: self.canonical_root.clone(),
                rel: String::new(),
            }
        } else {
            self.resolve_parts(&parts)?
        };
        if path.abs.is_dir() {
            Ok(path)
        } else {
            Err(PathError::Invalid(format!("not a directory: {}", path.rel)))
        }
    }

    fn resolve_parts(&self, parts: &[&str]) -> Result<RepoPath, PathError> {
        check_names(parts.iter().copied())?;
        let (existing, missing) = self.canonical_prefix(parts)?;
        // `canonicalize` resolved symlinks, junctions and NTFS aliases: check the real path.
        let inside = existing
            .strip_prefix(&self.canonical_root)
            .map_err(|_| PathError::EscapesRoot)?;
        let names: Vec<String> = inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        check_names(names.iter().map(String::as_str))?;
        let abs = missing.iter().fold(existing, |path, part| path.join(part));
        Ok(RepoPath {
            abs,
            rel: parts.join("/"),
        })
    }

    /// Canonicalizes the deepest existing ancestor of `parts`; returns it and the parts below
    /// it that don't exist yet.
    fn canonical_prefix<'p>(
        &self,
        parts: &'p [&'p str],
    ) -> Result<(PathBuf, &'p [&'p str]), PathError> {
        for depth in (0..=parts.len()).rev() {
            let candidate = parts[..depth]
                .iter()
                .fold(self.canonical_root.clone(), |path, part| path.join(part));
            if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                return Ok((canonical, &parts[depth..]));
            }
        }
        Err(PathError::Invalid("workspace root is gone".into()))
    }
}

/// Lexical validation: relative, `/`-separated, no `..`. Empty and `.` segments are dropped,
/// so `"."` yields no parts.
fn split_rel(rel: &str) -> Result<Vec<&str>, PathError> {
    if rel.contains('\0') {
        return Err(PathError::Invalid("path contains a NUL byte".into()));
    }
    if rel.contains('\\') {
        return Err(PathError::Invalid("use / as the path separator".into()));
    }
    let bytes = rel.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if rel.starts_with('/') || drive {
        return Err(PathError::Invalid(
            "path must be relative to the repository root".into(),
        ));
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(PathError::EscapesRoot),
            _ if part.chars().all(|c| c == '.') => {
                return Err(PathError::Invalid(format!("invalid path segment: {part}")));
            }
            // `name:stream` addresses an NTFS alternate data stream.
            _ if cfg!(windows) && part.contains(':') => {
                return Err(PathError::Invalid(format!("invalid path segment: {part}")));
            }
            _ => {
                let mut components = Path::new(part).components();
                let single_normal = matches!(
                    (components.next(), components.next()),
                    (Some(Component::Normal(_)), None)
                );
                if !single_normal {
                    return Err(PathError::Invalid(format!("invalid path segment: {part}")));
                }
                parts.push(part);
            }
        }
    }
    Ok(parts)
}

fn check_names<'a>(names: impl Iterator<Item = &'a str> + Clone) -> Result<(), PathError> {
    if names.clone().any(is_git_dir_name) {
        return Err(PathError::ForbiddenGitPath);
    }
    if names.into_iter().any(is_secret_name) {
        return Err(PathError::SecretFile);
    }
    Ok(())
}

/// Lowercased, without the trailing dots and spaces Windows ignores (`.git.` is `.git`).
fn normalized_name(name: &str) -> String {
    name.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

fn is_git_dir_name(name: &str) -> bool {
    normalized_name(name) == ".git"
}

fn is_secret_name(name: &str) -> bool {
    const ALLOWED_ENV: &[&str] = &[".env.example", ".env.sample", ".env.template"];
    const EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".kdbx"];
    const PREFIXES: &[&str] = &["id_rsa", "id_ed25519"];
    const NAMES: &[&str] = &[".npmrc", ".netrc", "secrets.yml", "secrets.yaml"];
    let name = normalized_name(name);
    let name = name.as_str();
    name == ".env"
        || (name.starts_with(".env.") && !ALLOWED_ENV.contains(&name))
        || EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || NAMES.contains(&name)
}

/// True when any component of `rel` (split on `/` or `\`) is `.git` or a secret file name,
/// case-insensitively. Tools use it to filter listings and search results.
pub fn is_secret_path(rel: &str) -> bool {
    rel.split(['/', '\\'])
        .any(|name| is_git_dir_name(name) || is_secret_name(name))
}

/// Produces a [`RepoWorkspace`] for a PR head. A trait so engine tests can use a fake.
#[async_trait]
pub trait RepoCloner: Send + Sync {
    async fn clone_pr(&self, spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError>;
}

/// `<temp dir>/adus-clones`, the parent of every workspace on this host.
pub fn clones_root() -> PathBuf {
    std::env::temp_dir().join("adus-clones")
}

/// Startup sweep of [`clones_root`]: removes workspaces older than one hour (left behind by a
/// crash). Never touches live clones of a parallel process. Returns the number removed.
pub async fn cleanup_orphaned_workspaces() -> usize {
    cleanup_orphaned_in(&clones_root(), ORPHAN_MAX_AGE).await
}

/// Removes `adus-*` entries of `root` whose mtime is at least `max_age` old. IO errors are
/// logged and skipped; a missing `root` removes nothing.
pub async fn cleanup_orphaned_in(root: &Path, max_age: Duration) -> usize {
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || sweep_orphans(&root, max_age)).await {
        Ok(removed) => removed,
        Err(e) => {
            tracing::warn!(error = %e, "orphaned workspace sweep failed");
            0
        }
    }
}

fn sweep_orphans(root: &Path, max_age: Duration) -> usize {
    // Never follow a symlinked root: it would delete `adus-*` entries somewhere else.
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::warn!(path = %root.display(), "clones root is a symlink; not sweeping it");
            return 0;
        }
        _ => {}
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(path = %root.display(), error = %e, "cannot list orphaned workspaces");
            return 0;
        }
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read orphaned workspace entry");
                continue;
            }
        };
        if !entry.file_name().to_string_lossy().starts_with("adus-") {
            continue;
        }
        let path = entry.path();
        match remove_if_old(&path, now, max_age) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot remove orphaned workspace");
            }
        }
    }
    removed
}

fn remove_if_old(path: &Path, now: SystemTime, max_age: Duration) -> std::io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO);
    if age < max_age {
        return Ok(false);
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(true)
}

/// Shallow-clones with the `git` CLI under the hardening described in the module docs.
#[derive(Debug, Clone)]
pub struct GitCliCloner {
    pub clone_timeout: Duration,
    pub max_bytes: u64,
    /// Parent directory of the workspaces ([`clones_root`] in production).
    pub clones_root: PathBuf,
    /// Allows `file://` URLs. Tests only.
    pub allow_file_protocol_for_test: bool,
}

impl Default for GitCliCloner {
    fn default() -> Self {
        Self {
            clone_timeout: CLONE_TIMEOUT,
            max_bytes: MAX_WORKSPACE_BYTES,
            clones_root: clones_root(),
            allow_file_protocol_for_test: false,
        }
    }
}

/// Paths of one workspace: `<dir>/repo` is the checkout, `<dir>/home` an empty `HOME`. Git runs
/// from `cwd` (the clones root, outside the workspace) with an explicit `--git-dir`, so a remote
/// helper that outlives a killed `git` never pins the workspace directory on Windows.
struct Layout {
    repo: PathBuf,
    git_dir: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
}

impl Layout {
    fn new(dir: &Path, cwd: &Path) -> Self {
        let repo = dir.join("repo");
        Self {
            git_dir: repo.join(".git"),
            repo,
            home: dir.join("home"),
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl RepoCloner for GitCliCloner {
    async fn clone_pr(&self, spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError> {
        self.check_url(&spec.url)?;
        let dir = create_workspace_dir(self.clones_root.clone()).await?;
        let layout = Layout::new(dir.path(), &self.clones_root);
        // On timeout the inner future is dropped, which kills its git child (`kill_on_drop`).
        let result = tokio::time::timeout(self.clone_timeout, self.populate(&layout, spec))
            .await
            .unwrap_or(Err(WorkspaceError::TimedOut));
        match result {
            Ok(()) => RepoWorkspace::with_root(dir, &layout.repo)
                .map_err(|e| unavailable("cannot open workspace", &e)),
            Err(e) => {
                remove_workspace(dir).await;
                Err(e)
            }
        }
    }
}

impl GitCliCloner {
    fn check_url(&self, url: &str) -> Result<(), WorkspaceError> {
        let lower = url.to_ascii_lowercase();
        let allowed = lower.starts_with("https://")
            || lower.starts_with("http://")
            || (self.allow_file_protocol_for_test && lower.starts_with("file://"));
        if allowed {
            Ok(())
        } else {
            Err(WorkspaceError::Unavailable(
                "unsupported clone URL scheme".into(),
            ))
        }
    }

    async fn populate(&self, layout: &Layout, spec: &CloneSpec) -> Result<(), WorkspaceError> {
        tokio::fs::create_dir(&layout.home)
            .await
            .map_err(|e| unavailable("cannot create workspace", &e))?;
        self.ensure_git_version(layout).await?;
        self.init(layout).await?;
        let redactor = Redactor::new(&spec.auth_header);
        self.fetch(layout, spec, &redactor).await?;
        self.checkout(layout).await?;
        self.check_head(layout, &spec.expected_sha).await?;

        let repo = layout.repo.clone();
        let max_bytes = self.max_bytes;
        let over = tokio::task::spawn_blocking(move || checkout_exceeds(&repo, max_bytes))
            .await
            .map_err(|_| WorkspaceError::Unavailable("size check failed".into()))?
            .map_err(|e| unavailable("size check failed", &e))?;
        if over {
            return Err(WorkspaceError::TooLarge);
        }
        Ok(())
    }

    /// `git --version` once per process; git below 2.31 cannot take the header from the env.
    async fn ensure_git_version(&self, layout: &Layout) -> Result<(), WorkspaceError> {
        static CHECKED: OnceLock<Result<(), WorkspaceError>> = OnceLock::new();
        if let Some(result) = CHECKED.get() {
            return result.clone();
        }
        let mut cmd = self.base_command(layout);
        cmd.arg("--version");
        // A failure to run git at all (missing binary, no file handles) is not cached.
        let out = run(cmd, "version", &Redactor::default()).await?;
        let result = match parse_git_version(&out) {
            Some(version) if version_supported(version) => Ok(()),
            Some(_) => Err(WorkspaceError::Unavailable(
                "git 2.31 or newer is required".into(),
            )),
            None => Err(WorkspaceError::Unavailable(
                "cannot parse git version".into(),
            )),
        };
        CHECKED.get_or_init(|| result).clone()
    }

    async fn init(&self, layout: &Layout) -> Result<(), WorkspaceError> {
        let mut cmd = self.git(layout);
        // Empty template: no sample hooks or other template files.
        cmd.args(["init", "--quiet", "--template=", "--"])
            .arg(&layout.repo);
        run(cmd, "init", &Redactor::default()).await.map(drop)
    }

    /// Fetches `spec.refspec`; on a non-auth failure retries once with the fallback refspec.
    async fn fetch(
        &self,
        layout: &Layout,
        spec: &CloneSpec,
        redactor: &Redactor,
    ) -> Result<(), WorkspaceError> {
        let primary = self.fetch_command(layout, spec, &spec.refspec);
        match (
            run(primary, "fetch", redactor).await,
            &spec.fallback_refspec,
        ) {
            (Ok(_), _) => Ok(()),
            (Err(WorkspaceError::Unauthorized), _) => Err(WorkspaceError::Unauthorized),
            (Err(_), Some(fallback)) => {
                let retry = self.fetch_command(layout, spec, fallback);
                run(retry, "fetch", redactor).await.map(drop)
            }
            (Err(e), None) => Err(e),
        }
    }

    /// The PAT goes only into this command's environment (`GIT_CONFIG_*`), never into argv.
    fn fetch_command(&self, layout: &Layout, spec: &CloneSpec, refspec: &str) -> Command {
        let mut cmd = self.git(layout);
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: {}", spec.auth_header),
            )
            .arg(flag_with_path("--git-dir=", &layout.git_dir))
            .args([
                "fetch",
                "--depth=1",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                &spec.url,
                refspec,
            ]);
        cmd
    }

    async fn checkout(&self, layout: &Layout) -> Result<(), WorkspaceError> {
        let mut cmd = self.git(layout);
        cmd.arg(flag_with_path("--git-dir=", &layout.git_dir))
            .arg(flag_with_path("--work-tree=", &layout.repo))
            .args(["checkout", "--detach", "--force", "FETCH_HEAD"]);
        run(cmd, "checkout", &Redactor::default()).await.map(drop)
    }

    /// A different HEAD (PR pushed during the clone) is only logged; no HEAD is an error.
    async fn check_head(&self, layout: &Layout, expected_sha: &str) -> Result<(), WorkspaceError> {
        let mut cmd = self.git(layout);
        cmd.arg(flag_with_path("--git-dir=", &layout.git_dir))
            .args(["rev-parse", "--verify", "HEAD"]);
        let head = run(cmd, "rev-parse", &Redactor::default()).await?;
        let head = head.trim();
        if head.is_empty() {
            return Err(WorkspaceError::Unavailable("checkout has no HEAD".into()));
        }
        if !head.eq_ignore_ascii_case(expected_sha) {
            tracing::warn!(
                expected = %expected_sha,
                actual = %head,
                "cloned HEAD differs from the PR head (pushed during the clone?)"
            );
        }
        Ok(())
    }

    /// `git` with the hardening `-c` flags. Every value is a constant of ours, never repo or
    /// user input.
    fn git(&self, layout: &Layout) -> Command {
        let protocol_file = if self.allow_file_protocol_for_test {
            "protocol.file.allow=always"
        } else {
            "protocol.file.allow=never"
        };
        let hooks_path = format!("core.hooksPath={NULL_DEVICE}");
        let low_speed_time = format!(
            "http.lowSpeedTime={}",
            self.clone_timeout
                .as_secs()
                .saturating_add(LOW_SPEED_GRACE_SECS)
        );
        let mut cmd = self.base_command(layout);
        for value in [
            hooks_path.as_str(),
            "core.symlinks=false",
            "core.fsmonitor=false",
            "filter.lfs.smudge=",
            "filter.lfs.process=",
            protocol_file,
            "http.followRedirects=false",
            "credential.helper=",
            "http.lowSpeedLimit=1",
            low_speed_time.as_str(),
        ] {
            cmd.args(["-c", value]);
        }
        cmd
    }

    /// Cleared environment plus the minimum git needs to start: no system or global config, no
    /// prompts, English messages (auth detection matches them).
    fn base_command(&self, layout: &Layout) -> Command {
        let mut cmd = Command::new("git");
        cmd.env_clear();
        for key in PASSTHROUGH_ENV {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.env("HOME", &layout.home)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("LC_ALL", "C");
        if cfg!(windows) {
            cmd.env("USERPROFILE", &layout.home);
        }
        cmd.current_dir(&layout.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

fn flag_with_path(flag: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(flag);
    arg.push(path);
    arg
}

fn unavailable(what: &str, e: &std::io::Error) -> WorkspaceError {
    WorkspaceError::Unavailable(format!("{what}: {}", e.kind()))
}

/// Runs `cmd` to completion and returns stdout. Failures carry scrubbed stderr.
async fn run(mut cmd: Command, step: &str, redactor: &Redactor) -> Result<String, WorkspaceError> {
    let output = cmd
        .output()
        .await
        .map_err(|e| unavailable(&format!("cannot run git {step}"), &e))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = redactor.scrub(&String::from_utf8_lossy(&output.stderr));
    Err(classify_failure(step, &stderr))
}

/// Maps already-scrubbed git stderr to an error.
fn classify_failure(step: &str, stderr: &str) -> WorkspaceError {
    let lower = stderr.to_ascii_lowercase();
    if AUTH_FAILURE_PHRASES.iter().any(|p| lower.contains(p)) {
        return WorkspaceError::Unauthorized;
    }
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let detail = lines[lines.len().saturating_sub(3)..].join("; ");
    let detail: String = detail.chars().take(MAX_ERROR_CHARS).collect();
    WorkspaceError::Unavailable(format!("git {step} failed: {detail}"))
}

/// Removes every form of the credential from text: the header, its base64 payload, the
/// decoded `user:password` pair and the password alone.
#[derive(Default)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn new(auth_header: &str) -> Self {
        let header = auth_header.trim();
        let mut secrets = vec![header.to_string()];
        if let Some((_, credential)) = header.split_once(' ') {
            let credential = credential.trim();
            secrets.push(credential.to_string());
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(credential)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            if let Some(pair) = decoded {
                if let Some((_, password)) = pair.split_once(':') {
                    secrets.push(password.to_string());
                }
                secrets.push(pair);
            }
        }
        secrets.retain(|s| !s.is_empty());
        // Longest first, so a secret is never left half-replaced by a shorter one.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    fn scrub(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_string(), |acc, secret| {
            acc.replace(secret.as_str(), "[redacted]")
        })
    }
}

fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let version = output.trim().strip_prefix("git version ")?;
    let mut parts = version.split('.');
    let mut number = || -> Option<u32> {
        let digits: String = parts
            .next()?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    };
    let major = number()?;
    let minor = number()?;
    Some((major, minor))
}

fn version_supported(version: (u32, u32)) -> bool {
    version >= MIN_GIT_VERSION
}

/// Creates `<root>/adus-{uuid}` (0700 on Unix) and, on Unix, checks afterwards that `root` is
/// still a private directory owned by the same user as the new workspace (see
/// [`verify_workspace_parent`]). The check runs before any git command; on failure the
/// `TempDir` is dropped and nothing is fetched.
async fn create_workspace_dir(root: PathBuf) -> Result<TempDir, WorkspaceError> {
    tokio::task::spawn_blocking(move || {
        ensure_private_root(&root)?;
        let prefix = format!("adus-{}", uuid::Uuid::new_v4());
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix).rand_bytes(0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let dir = builder.tempdir_in(&root)?;
        verify_workspace_parent(&root, dir.path())?;
        Ok(dir)
    })
    .await
    .map_err(|_| WorkspaceError::Unavailable("cannot create workspace".into()))?
    .map_err(|e| unavailable("cannot create workspace", &e))
}

/// Creates `root` if missing. On Unix it must then be a real directory (not a symlink) without
/// group or other permissions. This is only an early rejection: a predictable path under a
/// shared `/tmp` may belong to another user, who can change its mode or swap it for a symlink
/// at any time. The binding check is [`verify_workspace_parent`], after the workspace exists.
#[cfg(unix)]
fn ensure_private_root(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(root)?;
    let private = meta.file_type().is_dir() && meta.permissions().mode() & 0o077 == 0;
    if private {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "clones root is not a private directory",
        ))
    }
}

/// Windows: `%TEMP%` is already per user.
#[cfg(not(unix))]
fn ensure_private_root(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)
}

/// Unix: after creating `workspace` inside `root`, both must be real directories without group
/// or other permissions, and `root` must belong to the user that owns `workspace` (this
/// process). Only that user (or the superuser) can then have changed `root` or its entries, so
/// no one else can have redirected, replaced or planted files in the workspace.
#[cfg(unix)]
fn verify_workspace_parent(root: &Path, workspace: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let root_meta = std::fs::symlink_metadata(root)?;
    let ws_meta = std::fs::symlink_metadata(workspace)?;
    let ws_uid = ws_meta.uid();
    let root_ok = is_private_dir_of(
        root_meta.file_type().is_dir(),
        root_meta.mode(),
        root_meta.uid(),
        ws_uid,
    );
    let ws_ok = is_private_dir_of(ws_meta.file_type().is_dir(), ws_meta.mode(), ws_uid, ws_uid);
    if root_ok && ws_ok {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "clones root is not a private directory of this user",
        ))
    }
}

/// Windows: `%TEMP%` is already per user.
#[cfg(not(unix))]
fn verify_workspace_parent(_root: &Path, _workspace: &Path) -> std::io::Result<()> {
    Ok(())
}

/// True for a real directory (`is_dir` from `symlink_metadata`, so never a symlink) without
/// group or other permission bits, owned by `uid`.
#[cfg_attr(
    all(not(unix), not(test)),
    expect(dead_code, reason = "ownership is only checked on Unix")
)]
fn is_private_dir_of(is_dir: bool, mode: u32, owner_uid: u32, uid: u32) -> bool {
    is_dir && mode & 0o077 == 0 && owner_uid == uid
}

/// Deletes a failed workspace now. Retries briefly because Windows releases the files of a
/// just-killed process asynchronously; anything left is removed by the orphan sweep.
async fn remove_workspace(dir: TempDir) {
    let path = dir.path().to_path_buf();
    for attempt in 1..=REMOVE_ATTEMPTS {
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target)).await;
        match result {
            Ok(Ok(())) => break,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => break,
            Ok(Err(e)) if attempt == REMOVE_ATTEMPTS => {
                tracing::warn!(path = %path.display(), error = %e, "cannot remove failed workspace");
            }
            _ => tokio::time::sleep(REMOVE_RETRY_DELAY).await,
        }
    }
    drop(dir);
}

/// True when the checkout (without `.git`) holds more than `limit` bytes. Symlinks are not
/// followed.
fn checkout_exceeds(repo: &Path, limit: u64) -> std::io::Result<bool> {
    let mut total: u64 = 0;
    let mut stack = vec![repo.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if dir == repo && entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
                if total > limit {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::process::Command as StdCommand;
    use std::sync::{Arc, Mutex};

    use base64::Engine;

    use super::*;

    const PAT: &str = "s3cr3t-pat-value";

    fn auth_header() -> String {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{PAT}"));
        format!("Basic {encoded}")
    }

    /// Runs git for fixtures, isolated from the developer's own config.
    fn sh_git(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Shell hook that leaves `marker` behind if git ever runs it.
    fn hook_script(marker: &Path) -> String {
        let marker = marker.to_string_lossy().replace('\\', "/");
        format!("#!/bin/sh\necho fired > '{marker}'\n")
    }

    fn install_hook(git_dir: &Path, name: &str, marker: &Path) {
        let hooks = git_dir.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join(name);
        fs::write(&path, hook_script(marker)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    struct Source {
        dir: TempDir,
        head: String,
        markers: TempDir,
    }

    impl Source {
        fn url(&self) -> String {
            let p = self.dir.path().to_string_lossy().replace('\\', "/");
            if p.starts_with('/') {
                format!("file://{p}")
            } else {
                format!("file:///{p}")
            }
        }

        fn marker(&self) -> PathBuf {
            self.markers.path().join("hook-fired")
        }
    }

    /// Repo with a normal file, a symlink entry pointing outside, and hooks that would leave a
    /// marker if anything ran them.
    fn source_repo() -> Source {
        let dir = TempDir::new().unwrap();
        let markers = TempDir::new().unwrap();
        let p = dir.path();
        sh_git(p, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(p.join("src")).unwrap();
        fs::write(p.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(p.join(".env.example"), "KEY=\n").unwrap();
        // A symlink blob, created without needing OS symlink support.
        fs::write(p.join("link-target.txt"), "../outside").unwrap();
        let blob = sh_git(p, &["hash-object", "-w", "link-target.txt"]);
        fs::remove_file(p.join("link-target.txt")).unwrap();
        sh_git(
            p,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{blob},escape"),
            ],
        );
        sh_git(p, &["add", "src/main.rs", ".env.example"]);
        sh_git(p, &["commit", "-q", "-m", "init"]);
        let head = sh_git(p, &["rev-parse", "HEAD"]);
        let git_dir = p.join(".git");
        let source = Source { dir, head, markers };
        for hook in [
            "post-checkout",
            "post-merge",
            "pre-upload",
            "reference-transaction",
        ] {
            install_hook(&git_dir, hook, &source.marker());
        }
        source
    }

    /// A temp dir usable as `clones_root` (0700 on Unix, as the cloner requires).
    fn private_root() -> TempDir {
        let dir = TempDir::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    fn cloner(root: &Path) -> GitCliCloner {
        GitCliCloner {
            clones_root: root.to_path_buf(),
            allow_file_protocol_for_test: true,
            ..GitCliCloner::default()
        }
    }

    fn spec(url: &str, refspec: &str, fallback: Option<&str>, sha: &str) -> CloneSpec {
        CloneSpec {
            url: url.into(),
            refspec: refspec.into(),
            fallback_refspec: fallback.map(Into::into),
            expected_sha: sha.into(),
            auth_header: auth_header(),
        }
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    }

    /// Every file under `dir`, recursively.
    fn all_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    }

    #[tokio::test]
    async fn clones_local_repo_with_hardened_flags_and_deletes_on_drop() {
        let source = source_repo();
        let root = private_root();
        let url = source.url();

        let ws = cloner(root.path())
            .clone_pr(&spec(&url, &source.head, None, &source.head))
            .await
            .unwrap();

        let main = ws.resolve_existing_file("src/main.rs").unwrap();
        assert_eq!(fs::read_to_string(main.abs()).unwrap(), "fn main() {}\n");
        // core.symlinks=false: the symlink entry is checked out as a plain file.
        let escape = ws.root().join("escape");
        assert!(
            !fs::symlink_metadata(&escape)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&escape).unwrap(), "../outside");
        assert!(!source.marker().exists(), "a source repo hook ran");

        let git_dir = ws.root().join(".git");
        let head = fs::read_to_string(git_dir.join("HEAD")).unwrap();
        assert_eq!(head.trim(), source.head);
        let config = fs::read(git_dir.join("config")).unwrap();
        for needle in ["remote", "extraheader", "authorization", url.as_str()] {
            assert!(!contains(&config, needle), ".git/config contains {needle}");
        }
        let header = auth_header();
        for file in all_files(&git_dir) {
            let bytes = fs::read(&file).unwrap();
            assert!(!contains(&bytes, PAT), "{} has the PAT", file.display());
            assert!(
                !contains(&bytes, &header),
                "{} has the header",
                file.display()
            );
        }

        let path = ws.root().to_path_buf();
        drop(ws);
        assert!(!path.exists());
        assert!(entries(root.path()).is_empty());
    }

    #[tokio::test]
    async fn hooks_in_workspace_do_not_fire_during_checkout() {
        let source = source_repo();
        let root = private_root();
        let dir = TempDir::new_in(root.path()).unwrap();
        let cloner = cloner(root.path());
        let layout = Layout::new(dir.path(), root.path());
        let marker = source.marker();
        let spec = spec(&source.url(), &source.head, None, &source.head);
        let redactor = Redactor::new(&spec.auth_header);

        fs::create_dir(&layout.home).unwrap();
        cloner.init(&layout).await.unwrap();
        install_hook(&layout.git_dir, "post-checkout", &marker);
        cloner.fetch(&layout, &spec, &redactor).await.unwrap();
        cloner.checkout(&layout).await.unwrap();
        assert!(
            !marker.exists(),
            "workspace hook ran during hardened checkout"
        );

        // Control: plain git does run the planted hook, so the check above means something.
        sh_git(&layout.repo, &["checkout", "-q", "--detach", "HEAD"]);
        assert!(marker.exists());
    }

    #[test]
    fn fetch_puts_auth_header_in_env_not_argv() {
        let root = private_root();
        let cloner = GitCliCloner {
            clones_root: root.path().to_path_buf(),
            ..GitCliCloner::default()
        };
        let layout = Layout::new(&root.path().join("adus-x"), root.path());
        let spec = spec(
            "https://github.com/o/r.git",
            "refs/pull/7/head",
            None,
            "abc",
        );

        let cmd = cloner.fetch_command(&layout, &spec, "refs/pull/7/head");
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let header = auth_header();
        for arg in &args {
            assert!(
                !arg.contains(PAT) && !arg.contains(&header),
                "secret in argv"
            );
        }
        for flag in [
            "core.hooksPath=/dev/null",
            "core.symlinks=false",
            "core.fsmonitor=false",
            "filter.lfs.smudge=",
            "filter.lfs.process=",
            "protocol.file.allow=never",
        ] {
            let i = args.iter().position(|a| a == flag).expect(flag);
            assert_eq!(args[i - 1], "-c");
        }
        let n = args.len();
        assert_eq!(
            args[n - 3..],
            ["--", "https://github.com/o/r.git", "refs/pull/7/head"]
        );
        assert!(args.contains(&"--depth=1".to_string()));
        assert!(args.contains(&"--no-recurse-submodules".to_string()));

        let envs: Vec<(String, String)> = std_cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        let get = |key: &str| envs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(get("GIT_CONFIG_KEY_0").as_deref(), Some("http.extraHeader"));
        assert_eq!(
            get("GIT_CONFIG_VALUE_0"),
            Some(format!("Authorization: {header}"))
        );
        assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        for (key, value) in &envs {
            if key != "GIT_CONFIG_VALUE_0" {
                assert!(!value.contains(PAT), "PAT leaked into {key}");
            }
        }
    }

    #[tokio::test]
    async fn falls_back_to_fallback_refspec_when_primary_fetch_fails() {
        let source = source_repo();
        let root = private_root();
        let url = source.url();

        let ws = cloner(root.path())
            .clone_pr(&spec(
                &url,
                "refs/pull/999/head",
                Some("refs/heads/main"),
                &source.head,
            ))
            .await
            .unwrap();
        let head = fs::read_to_string(ws.root().join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), source.head);

        let err = cloner(root.path())
            .clone_pr(&spec(&url, "refs/pull/999/head", None, &source.head))
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Unavailable(_)), "{err:?}");
        drop(ws);
        assert!(entries(root.path()).is_empty());
    }

    #[tokio::test]
    async fn head_mismatch_with_expected_sha_still_succeeds() {
        let source = source_repo();
        let root = private_root();
        let other = "0123456789abcdef0123456789abcdef01234567";
        let ws = cloner(root.path())
            .clone_pr(&spec(&source.url(), "refs/heads/main", None, other))
            .await;
        assert!(ws.is_ok(), "{ws:?}");
    }

    #[tokio::test]
    async fn rejects_checkout_over_max_bytes() {
        let source = source_repo();
        let root = private_root();
        let cloner = GitCliCloner {
            max_bytes: 8,
            ..cloner(root.path())
        };
        let err = cloner
            .clone_pr(&spec(&source.url(), &source.head, None, &source.head))
            .await
            .unwrap_err();
        assert_eq!(err, WorkspaceError::TooLarge);
        assert!(entries(root.path()).is_empty());
    }

    #[tokio::test]
    async fn rejects_urls_outside_http_and_https() {
        let source = source_repo();
        let root = private_root();
        let strict = GitCliCloner {
            clones_root: root.path().to_path_buf(),
            ..GitCliCloner::default()
        };
        for url in [
            source.url(),
            "ext::sh -c touch% /tmp/x".into(),
            "ssh://h/r".into(),
        ] {
            let err = strict
                .clone_pr(&spec(&url, "refs/heads/main", None, &source.head))
                .await
                .unwrap_err();
            assert!(
                matches!(err, WorkspaceError::Unavailable(_)),
                "{url}: {err:?}"
            );
        }
        assert!(entries(root.path()).is_empty());
    }

    fn workspace_with_files(files: &[&str]) -> RepoWorkspace {
        let dir = TempDir::new().unwrap();
        for f in files {
            let path = dir.path().join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        RepoWorkspace::from_temp_dir(dir).unwrap()
    }

    #[test]
    fn resolve_rejects_traversal_git_and_secret_files() {
        let ws = workspace_with_files(&[
            "src/main.rs",
            ".env",
            ".env.example",
            ".env.production",
            "certs/server.pem",
            "keys/id_rsa",
            ".git/HEAD",
        ]);
        let cases: &[(&str, PathError)] = &[
            ("../etc/passwd", PathError::EscapesRoot),
            ("src/../../x", PathError::EscapesRoot),
            ("src/..", PathError::EscapesRoot),
            (".git/HEAD", PathError::ForbiddenGitPath),
            (".GIT/config", PathError::ForbiddenGitPath),
            ("sub/.git", PathError::ForbiddenGitPath),
            (".git./HEAD", PathError::ForbiddenGitPath),
            (".env", PathError::SecretFile),
            (".env.production", PathError::SecretFile),
            ("certs/server.pem", PathError::SecretFile),
            ("keys/id_rsa", PathError::SecretFile),
            ("KEYS/ID_RSA.pub", PathError::SecretFile),
            ("app/.npmrc", PathError::SecretFile),
            ("config/secrets.yml", PathError::SecretFile),
            ("vault.KDBX", PathError::SecretFile),
        ];
        for (rel, want) in cases {
            assert_eq!(ws.resolve(rel).as_ref().err(), Some(want), "{rel}");
        }
        for rel in [
            "/etc/passwd",
            "C:/Windows",
            "c:x",
            "src\\main.rs",
            "",
            ".",
            "./",
            "...",
            "a\0b",
        ] {
            assert!(
                matches!(ws.resolve(rel), Err(PathError::Invalid(_))),
                "{rel:?}"
            );
        }

        let ok = ws.resolve("./src//main.rs").unwrap();
        assert_eq!(ok.rel(), "src/main.rs");
        assert!(ok.abs().starts_with(ws.root()));
        assert!(ok.abs().is_file());
        assert_eq!(ws.resolve(".env.example").unwrap().rel(), ".env.example");
        // Missing files resolve; callers report "not found" themselves.
        let missing = ws.resolve("src/new/file.rs").unwrap();
        assert!(missing.abs().starts_with(ws.root()));
        assert!(!missing.abs().exists());

        assert!(ws.resolve_existing_file("src/main.rs").is_ok());
        assert!(matches!(
            ws.resolve_existing_file("src"),
            Err(PathError::Invalid(_))
        ));
        assert!(matches!(
            ws.resolve_existing_file("nope.rs"),
            Err(PathError::Invalid(_))
        ));

        for rel in ["", ".", "./"] {
            let root = ws.resolve_dir(rel).unwrap();
            assert_eq!(root.abs(), ws.root());
            assert_eq!(root.rel(), "");
        }
        assert_eq!(ws.resolve_dir("src/").unwrap().rel(), "src");
        assert!(matches!(
            ws.resolve_dir("src/main.rs"),
            Err(PathError::Invalid(_))
        ));
        assert_eq!(
            ws.resolve_dir(".git").err(),
            Some(PathError::ForbiddenGitPath)
        );
    }

    #[test]
    fn is_secret_path_matches_components_case_insensitively() {
        for p in [
            ".git",
            "a/.git/config",
            ".env",
            "deploy/.ENV.local",
            "x/tls.key",
            "x/cert.P12",
            "x/cert.pfx",
            "home/id_ed25519.pub",
            ".netrc",
            "config/secrets.yaml",
            "foo.pem/readme.md",
        ] {
            assert!(is_secret_path(p), "{p}");
        }
        for p in [
            "src/main.rs",
            ".env.example",
            ".env.sample",
            "a/.env.template",
            "docs/keys.md",
            "environment.rs",
            "secrets.rs",
        ] {
            assert!(!is_secret_path(p), "{p}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escaping_root() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("passwd"), "root").unwrap();
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("passwd"), dir.path().join("file-link"))
            .unwrap();
        let ws = RepoWorkspace::from_temp_dir(dir).unwrap();
        for rel in ["link", "link/passwd", "link/missing.rs", "file-link"] {
            assert_eq!(ws.resolve(rel).err(), Some(PathError::EscapesRoot), "{rel}");
        }
    }

    #[test]
    fn private_dir_check_requires_directory_mode_and_owner() {
        assert!(is_private_dir_of(true, 0o40700, 1000, 1000));
        assert!(is_private_dir_of(true, 0o700, 1000, 1000));
        assert!(is_private_dir_of(true, 0o500, 1000, 1000));
        // Owned by another user, even at 0700 (the race in which an owner flips the mode).
        assert!(!is_private_dir_of(true, 0o700, 1001, 1000));
        // Group or other bits.
        assert!(!is_private_dir_of(true, 0o750, 1000, 1000));
        assert!(!is_private_dir_of(true, 0o701, 1000, 1000));
        assert!(!is_private_dir_of(true, 0o777, 1000, 1000));
        // A symlink or file (`symlink_metadata` reports not-a-directory).
        assert!(!is_private_dir_of(false, 0o700, 1000, 1000));
    }

    #[cfg(unix)]
    #[test]
    fn verify_workspace_parent_accepts_private_and_rejects_shared_root() {
        use std::os::unix::fs::PermissionsExt;
        let root = private_root();
        let ws = root.path().join("adus-ws");
        fs::create_dir(&ws).unwrap();
        fs::set_permissions(&ws, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(verify_workspace_parent(root.path(), &ws).is_ok());

        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify_workspace_parent(root.path(), &ws).is_err());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(&ws, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_workspace_parent(root.path(), &ws).is_err());
        let missing = root.path().join("gone");
        assert!(verify_workspace_parent(root.path(), &missing).is_err());
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clones_root_and_workspace_are_private() {
        let source = source_repo();
        let parent = private_root();
        let clones = parent.path().join("adus-clones");
        let ws = cloner(&clones)
            .clone_pr(&spec(&source.url(), &source.head, None, &source.head))
            .await
            .unwrap();
        assert_eq!(mode(&clones), 0o700);
        let workspace_dir = ws.root().parent().unwrap();
        assert_eq!(mode(workspace_dir), 0o700);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_symlinked_or_shared_clones_root() {
        use std::os::unix::fs::PermissionsExt;
        let source = source_repo();
        let parent = private_root();
        let target = parent.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let shared = parent.path().join("shared");
        fs::create_dir(&shared).unwrap();

        for (root, perms) in [
            (&link, None),
            (&shared, Some(0o777)),
            (&shared, Some(0o750)),
        ] {
            if let Some(perms) = perms {
                fs::set_permissions(root, fs::Permissions::from_mode(perms)).unwrap();
            }
            let err = cloner(root)
                .clone_pr(&spec(&source.url(), &source.head, None, &source.head))
                .await
                .unwrap_err();
            assert!(
                matches!(err, WorkspaceError::Unavailable(_)),
                "{root:?}: {err:?}"
            );
        }
        assert!(entries(&target).is_empty());
        assert!(entries(&shared).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_refuses_symlinked_clones_root() {
        let parent = private_root();
        let target = parent.path().join("target");
        fs::create_dir_all(target.join("adus-x")).unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(cleanup_orphaned_in(&link, Duration::ZERO).await, 0);
        assert!(target.join("adus-x").exists());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_rejects_junction_escaping_root() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("passwd"), "root").unwrap();
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("link");
        // Junctions need no privilege, unlike symlinks.
        let out = StdCommand::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let ws = RepoWorkspace::from_temp_dir(dir).unwrap();
        for rel in ["link", "link/passwd", "link/missing.rs"] {
            assert_eq!(ws.resolve(rel).err(), Some(PathError::EscapesRoot), "{rel}");
        }
    }

    #[test]
    fn scrubs_auth_header_from_git_errors() {
        let header = auth_header();
        let encoded = header.trim_start_matches("Basic ");
        let redactor = Redactor::new(&header);
        let stderr = format!(
            "fatal: unable to access: header {header}\nraw {PAT} pair x-access-token:{PAT} b64 {encoded}\n"
        );
        let err = classify_failure("fetch", &redactor.scrub(&stderr));
        let WorkspaceError::Unavailable(msg) = err else {
            panic!("expected Unavailable, got {err:?}");
        };
        assert!(!msg.contains(PAT), "{msg}");
        assert!(!msg.contains(encoded), "{msg}");
        assert!(msg.contains("unable to access"), "{msg}");
    }

    #[test]
    fn maps_git_auth_failures_to_unauthorized_only() {
        for stderr in [
            "fatal: Authentication failed for 'https://github.com/o/r.git/'",
            "remote: Invalid username or password.",
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "fatal: unable to access 'https://x/': The requested URL returned error: 401",
            "fatal: unable to access 'https://x/': The requested URL returned error: 403",
        ] {
            assert_eq!(
                classify_failure("fetch", stderr),
                WorkspaceError::Unauthorized,
                "{stderr}"
            );
        }
        for stderr in [
            "fatal: couldn't find remote ref 401abc",
            "fatal: couldn't find remote ref refs/heads/fix-403",
            "fatal: unable to access 'https://x/': Could not resolve host: x",
            "fatal: The requested URL returned error: 404",
        ] {
            assert!(
                matches!(
                    classify_failure("fetch", stderr),
                    WorkspaceError::Unavailable(_)
                ),
                "{stderr}"
            );
        }
    }

    /// HTTP server that records each request head and answers 401.
    fn spawn_401_server() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/o/r.git", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn fetch_sends_auth_header_and_maps_401_to_unauthorized() {
        let (url, seen) = spawn_401_server();
        let root = private_root();
        let err = cloner(root.path())
            .clone_pr(&spec(
                &url,
                "refs/pull/1/head",
                Some("refs/heads/main"),
                "abc",
            ))
            .await
            .unwrap_err();
        assert_eq!(err, WorkspaceError::Unauthorized);
        let requests = seen.lock().unwrap().join("\n");
        assert!(
            requests.contains(&format!("Authorization: {}", auth_header())),
            "{requests}"
        );
        assert!(entries(root.path()).is_empty());
    }

    #[tokio::test]
    async fn timeout_aborts_clone_and_cleans_up_tempdir() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/o/r.git", listener.local_addr().unwrap());
        // Accept connections and never answer.
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                held.push(stream);
            }
        });
        let root = private_root();
        let cloner = GitCliCloner {
            clone_timeout: Duration::from_secs(2),
            ..cloner(root.path())
        };

        let started = std::time::Instant::now();
        let err = cloner
            .clone_pr(&spec(&url, "refs/pull/1/head", None, "abc"))
            .await
            .unwrap_err();
        assert_eq!(err, WorkspaceError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            entries(root.path()).is_empty(),
            "{:?}",
            entries(root.path())
        );
    }

    #[tokio::test]
    async fn cleanup_orphaned_workspaces_removes_leftover_dirs() {
        let root = private_root();
        fs::create_dir_all(root.path().join("adus-a/repo/src")).unwrap();
        fs::write(root.path().join("adus-a/repo/src/x.rs"), "x").unwrap();
        fs::write(root.path().join("adus-b"), "stray file").unwrap();
        fs::create_dir(root.path().join("not-ours")).unwrap();

        // Fresh entries survive the production age limit.
        assert_eq!(
            cleanup_orphaned_in(root.path(), Duration::from_secs(3600)).await,
            0
        );
        assert_eq!(entries(root.path()).len(), 3);

        assert_eq!(cleanup_orphaned_in(root.path(), Duration::ZERO).await, 2);
        assert_eq!(entries(root.path()), vec![root.path().join("not-ours")]);

        let missing = root.path().join("missing");
        assert_eq!(cleanup_orphaned_in(&missing, Duration::ZERO).await, 0);
    }

    #[test]
    fn parses_git_version() {
        assert_eq!(
            parse_git_version("git version 2.56.1.windows.1\n"),
            Some((2, 56))
        );
        assert_eq!(parse_git_version("git version 2.30.0"), Some((2, 30)));
        assert_eq!(parse_git_version("git version 3.0"), Some((3, 0)));
        assert_eq!(parse_git_version("nope"), None);
        assert!(version_supported((2, 31)));
        assert!(version_supported((3, 0)));
        assert!(!version_supported((2, 30)));
    }
}
