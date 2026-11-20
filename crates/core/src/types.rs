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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "severity", rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
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
}
