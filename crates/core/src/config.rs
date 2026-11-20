use crate::crypto::Cipher;
use crate::error::ConfigError;
use std::collections::HashMap;

use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub secret_key: String,
    pub public_url: String,
    pub bind_addr: String,
    pub workers: usize,
}

impl Config {
    pub fn from_env() -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Config::from_map(&vars)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Config, ConfigError> {
        let workers = match vars.get("ADUS_WORKERS") {
            None => 2,
            Some(raw) => raw.parse().map_err(|_| ConfigError::Invalid {
                name: "ADUS_WORKERS",
                reason: format!("`{raw}` is not a number"),
            })?,
        };
        Ok(Config {
            database_url: required(vars, "DATABASE_URL")?,
            redis_url: required(vars, "REDIS_URL")?,
            secret_key: required(vars, "ADUS_SECRET_KEY")?,
            public_url: required(vars, "ADUS_PUBLIC_URL")?
                .trim_end_matches('/')
                .to_string(),
            bind_addr: vars
                .get("ADUS_BIND_ADDR")
                .cloned()
                .unwrap_or_else(|| "0.0.0.0:8080".to_string()),
            workers,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
}

/// Satu temuan `adus check-config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    pub level: Level,
    pub var: &'static str,
    pub message: String,
}

/// Periksa semua variabel sekaligus (bukan berhenti di error pertama seperti `from_map`).
/// Tidak pernah memuat nilai rahasia di pesan.
pub fn check(map: &HashMap<String, String>) -> Vec<ConfigIssue> {
    let mut issues = Vec::new();
    let mut issue = |level, var, message: &str| {
        issues.push(ConfigIssue {
            level,
            var,
            message: message.to_string(),
        })
    };
    let value = |name: &str| map.get(name).map(|v| v.trim()).filter(|v| !v.is_empty());

    for var in [
        "DATABASE_URL",
        "REDIS_URL",
        "ADUS_SECRET_KEY",
        "ADUS_PUBLIC_URL",
    ] {
        if value(var).is_none() {
            issue(Level::Error, var, "is required but empty or missing");
        }
    }
    if let Some(key) = value("ADUS_SECRET_KEY")
        && Cipher::from_base64_key(key).is_err()
    {
        issue(
            Level::Error,
            "ADUS_SECRET_KEY",
            "must be base64 of exactly 32 bytes (generate one with `adus gen-secret`)",
        );
    }
    if let Some(raw) = value("ADUS_PUBLIC_URL") {
        match Url::parse(raw) {
            Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => {
                if url.scheme() != "https" {
                    issue(
                        Level::Warn,
                        "ADUS_PUBLIC_URL",
                        "is not https; the session cookie will not be Secure",
                    );
                }
                if matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "0.0.0.0")) {
                    issue(
                        Level::Warn,
                        "ADUS_PUBLIC_URL",
                        "points to this machine; GitHub/Azure cannot send webhooks to this address",
                    );
                }
            }
            _ => issue(
                Level::Error,
                "ADUS_PUBLIC_URL",
                "must be a valid http:// or https:// URL",
            ),
        }
    }
    issues
}

/// Teks keluaran `adus check-config`: satu baris per temuan, lalu `config OK` jika tidak ada error.
pub fn format_issues(issues: &[ConfigIssue]) -> String {
    let mut out = String::new();
    for i in issues {
        let tag = match i.level {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
        };
        out.push_str(&format!(
            "{tag} {}: {}
",
            i.var, i.message
        ));
    }
    if !issues.iter().any(|i| i.level == Level::Error) {
        out.push_str(
            "config OK
",
        );
    }
    out
}

fn required(vars: &HashMap<String, String>, name: &'static str) -> Result<String, ConfigError> {
    vars.get(name).cloned().ok_or(ConfigError::Missing(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn full_map_without(skip: &[&str]) -> HashMap<String, String> {
        let all = [
            ("DATABASE_URL", "postgres://x"),
            ("REDIS_URL", "redis://x"),
            ("ADUS_SECRET_KEY", "k"),
            ("ADUS_PUBLIC_URL", "http://x"),
            ("ADUS_BIND_ADDR", "127.0.0.1:9000"),
            ("ADUS_WORKERS", "7"),
        ];
        let mut m = map(&all);
        for s in skip {
            m.remove(*s);
        }
        m
    }

    const GOOD_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn good(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut m = map(&[
            ("DATABASE_URL", "postgres://x"),
            ("REDIS_URL", "redis://x"),
            ("ADUS_SECRET_KEY", GOOD_KEY),
            ("ADUS_PUBLIC_URL", "https://adus.example.com"),
        ]);
        m.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        m
    }

    fn has(issues: &[ConfigIssue], level: Level, var: &str) -> bool {
        issues.iter().any(|i| i.level == level && i.var == var)
    }

    #[test]
    fn check_reports_missing_required() {
        let issues = check(&HashMap::new());
        for var in [
            "DATABASE_URL",
            "REDIS_URL",
            "ADUS_SECRET_KEY",
            "ADUS_PUBLIC_URL",
        ] {
            assert!(has(&issues, Level::Error, var), "{var}");
        }
        assert_eq!(issues.len(), 4);
        assert!(check(&good(&[])).is_empty());
    }

    #[test]
    fn check_rejects_short_secret_key() {
        // 16 byte, bukan 32; nilainya tidak boleh muncul di pesan.
        let issues = check(&good(&[("ADUS_SECRET_KEY", "AAAAAAAAAAAAAAAAAAAAAA==")]));
        assert!(has(&issues, Level::Error, "ADUS_SECRET_KEY"));
        assert!(issues.iter().all(|i| !i.message.contains("AAAAAAAA")));
        let issues = check(&good(&[("ADUS_SECRET_KEY", "not base64!!")]));
        assert!(has(&issues, Level::Error, "ADUS_SECRET_KEY"));
    }

    #[test]
    fn check_warns_on_http_and_localhost() {
        let issues = check(&good(&[("ADUS_PUBLIC_URL", "http://localhost:8080")]));
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|i| i.level == Level::Warn));
        assert!(issues.iter().any(|i| i.message.contains("webhooks")));
        let issues = check(&good(&[("ADUS_PUBLIC_URL", "ftp://adus.example.com")]));
        assert!(has(&issues, Level::Error, "ADUS_PUBLIC_URL"));
        let issues = check(&good(&[("ADUS_PUBLIC_URL", "https://0.0.0.0")]));
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn check_config_ok_prints_config_ok() {
        assert_eq!(
            format_issues(&check(&good(&[]))),
            "config OK
"
        );
        let warn_only = format_issues(&check(&good(&[("ADUS_PUBLIC_URL", "http://adus.test")])));
        assert!(warn_only.starts_with("WARN ADUS_PUBLIC_URL: "));
        assert!(warn_only.ends_with(
            "config OK
"
        ));
        let bad = format_issues(&check(&HashMap::new()));
        assert!(bad.contains("ERROR DATABASE_URL: "));
        assert!(!bad.contains("config OK"));
    }

    #[test]
    fn missing_database_url_is_error() {
        let err = Config::from_map(&map(&[
            ("REDIS_URL", "redis://x"),
            ("ADUS_SECRET_KEY", "k"),
            ("ADUS_PUBLIC_URL", "http://x"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn defaults_bind_addr_and_workers() {
        let c = Config::from_map(&full_map_without(&["ADUS_BIND_ADDR", "ADUS_WORKERS"])).unwrap();
        assert_eq!(c.bind_addr, "0.0.0.0:8080");
        assert_eq!(c.workers, 2);
    }

    #[test]
    fn explicit_bind_addr_and_workers_are_used() {
        let c = Config::from_map(&full_map_without(&[])).unwrap();
        assert_eq!(c.bind_addr, "127.0.0.1:9000");
        assert_eq!(c.workers, 7);
    }

    #[test]
    fn invalid_workers_is_error() {
        let mut m = full_map_without(&[]);
        m.insert("ADUS_WORKERS".into(), "abc".into());
        let err = Config::from_map(&m).unwrap_err();
        assert!(err.to_string().contains("ADUS_WORKERS"));
    }

    #[test]
    fn public_url_trailing_slash_trimmed() {
        let mut m = full_map_without(&[]);
        m.insert("ADUS_PUBLIC_URL".into(), "https://a.b/".into());
        assert_eq!(Config::from_map(&m).unwrap().public_url, "https://a.b");
    }
}
