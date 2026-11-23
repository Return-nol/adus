use serde::{Deserialize, Serialize};

/// Penanda di setiap komentar yang diposting Adus. Komentar yang memuatnya diabaikan.
pub const ADUDU_MARKER: &str = "<!-- adudu -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "connection_kind", rename_all = "snake_case")]
pub enum ConnectionKind {
    AzureDevops,
    Github,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "provider_kind", rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Gemini,
    OpenaiCompatible,
}

/// Urutan deklarasi = urutan keparahan (`Info < Warning < Critical`), dipakai sebagai kunci `BTreeMap`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "severity", rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// How a PR is reviewed. Stored in `settings.review_mode` and `repositories.review_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "review_mode", rename_all = "snake_case")]
pub enum ReviewMode {
    /// One reviewer pass over the whole diff.
    #[default]
    Generalist,
    /// Several focused passes (bugs, security, performance, rules).
    Specialists,
}

/// Whether a repository learns "by design" exceptions from closed PRs.
/// Stored in `repositories.learning_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "learning_mode", rename_all = "snake_case")]
pub enum LearningMode {
    /// Never learn.
    Off,
    /// Learned memories wait for an admin (`pending`).
    #[default]
    Approval,
    /// Learned memories are active at once when a registered Adus user defended them.
    Auto,
}

/// Path file di dalam Adus tidak punya `/` di depan (Azure mengembalikan `/src/a.rs`).
pub fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_strips_leading_slash() {
        assert_eq!(normalize_path("/src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src/a.rs"), "src/a.rs");
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ConnectionKind::AzureDevops).unwrap(),
            "\"azure_devops\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenaiCompatible).unwrap(),
            "\"openai_compatible\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
    }

    #[test]
    fn review_mode_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReviewMode::Generalist).unwrap(),
            "\"generalist\""
        );
        assert_eq!(
            serde_json::to_string(&ReviewMode::Specialists).unwrap(),
            "\"specialists\""
        );
        assert_eq!(ReviewMode::default(), ReviewMode::Generalist);
    }
}
