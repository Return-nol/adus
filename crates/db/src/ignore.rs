//! Daftar file yang dilewati reviewer: validasi pola, pencocokan path, dan kata kunci judul PR.

use adus_core::{default_ignore::DEFAULT_IGNORE_PATHS, types::normalize_path};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::settings::Settings;

const MAX_PATTERN_LEN: usize = 500;
const MAX_PATTERNS: usize = 5000;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum IgnoreError {
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(String),
    #[error("pattern is longer than {MAX_PATTERN_LEN} characters: {0}")]
    TooLong(String),
    #[error("more than {MAX_PATTERNS} patterns")]
    TooMany,
    /// Tiap pola valid, tetapi gabungannya melewati batas ukuran regex `GlobSet`.
    #[error("the ignore list is too large or complex to compile; use fewer or simpler patterns")]
    TooComplex,
}

/// Daftar bawaan Kodus sebagai `Vec<String>`.
pub fn default_ignore_paths() -> Vec<String> {
    DEFAULT_IGNORE_PATHS.iter().map(|p| p.to_string()).collect()
}

/// Daftar yang tersimpan, atau daftar bawaan Kodus bila belum pernah disimpan (NULL).
pub fn effective_ignore_paths(settings: &Settings) -> Vec<String> {
    match &settings.ignore_paths {
        Some(paths) => paths.clone(),
        None => default_ignore_paths(),
    }
}

/// Trim, buang `/` di depan (path Adus tidak pernah diawali `/`), buang entri kosong dan duplikat
/// (urutan tetap), lalu validasi tiap pola.
pub fn normalize_patterns(raw: &[String]) -> Result<Vec<String>, IgnoreError> {
    let mut patterns: Vec<String> = Vec::new();
    for pattern in raw {
        let pattern = pattern.trim().trim_start_matches('/');
        if pattern.is_empty() || patterns.iter().any(|p| p == pattern) {
            continue;
        }
        if pattern.chars().count() > MAX_PATTERN_LEN {
            return Err(IgnoreError::TooLong(pattern.to_string()));
        }
        Glob::new(pattern).map_err(|_| IgnoreError::InvalidGlob(pattern.to_string()))?;
        patterns.push(pattern.to_string());
        if patterns.len() > MAX_PATTERNS {
            return Err(IgnoreError::TooMany);
        }
    }
    Ok(patterns)
}

/// Pola yang sudah dikompilasi. Semantik glob sama dengan rule (`Glob::new` bawaan).
pub struct IgnoreMatcher {
    patterns: Vec<String>,
    set: GlobSet,
}

impl IgnoreMatcher {
    pub fn new(patterns: &[String]) -> Result<Self, IgnoreError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|_| IgnoreError::InvalidGlob(pattern.clone()))?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|_| IgnoreError::TooComplex)?;
        Ok(Self {
            patterns: patterns.to_vec(),
            set,
        })
    }

    /// Pola pertama (urutan daftar) yang cocok dengan path; path dinormalisasi (tanpa `/` di depan).
    pub fn matching_pattern(&self, path: &str) -> Option<&str> {
        // `matches` mengembalikan indeks terurut naik, jadi yang pertama = pola paling awal.
        let first = *self.set.matches(normalize_path(path)).first()?;
        Some(&self.patterns[first])
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.set.is_match(normalize_path(path))
    }
}

/// Judul PR memuat salah satu kata kunci (tanpa membedakan huruf besar/kecil). Kata kunci kosong diabaikan.
pub fn title_matches(title: &str, keywords: &[String]) -> bool {
    let title = title.to_lowercase();
    keywords
        .iter()
        .filter(|k| !k.is_empty())
        .any(|k| title.contains(&k.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn defaults() -> Vec<String> {
        strings(DEFAULT_IGNORE_PATHS)
    }

    #[test]
    fn defaults_load_and_compile() {
        assert!(DEFAULT_IGNORE_PATHS.len() > 1000);
        assert!(IgnoreMatcher::new(&defaults()).is_ok());
    }

    #[test]
    fn defaults_ignore_lock_and_media() {
        let matcher = IgnoreMatcher::new(&defaults()).unwrap();
        for path in [
            "yarn.lock",
            "package-lock.json",
            "assets/logo.png",
            "/src/a.json",
        ] {
            assert!(matcher.is_ignored(path), "{path} should be ignored");
        }
        assert!(!matcher.is_ignored("src/main.rs"));
    }

    #[test]
    fn matching_pattern_reports_pattern() {
        let matcher = IgnoreMatcher::new(&strings(&["dist/**"])).unwrap();
        assert_eq!(matcher.matching_pattern("dist/a.js"), Some("dist/**"));
        assert_eq!(matcher.matching_pattern("src/a.js"), None);
    }

    #[test]
    fn matching_pattern_returns_first_in_list_order() {
        let matcher = IgnoreMatcher::new(&strings(&["**/*.js", "dist/**"])).unwrap();
        assert_eq!(matcher.matching_pattern("dist/a.js"), Some("**/*.js"));
    }

    #[test]
    fn normalize_rejects_bad_glob() {
        assert_eq!(
            normalize_patterns(&strings(&["ok/**", "a[b"])),
            Err(IgnoreError::InvalidGlob("a[b".into()))
        );
    }

    #[test]
    fn normalize_trims_dedups_drops_empty() {
        let raw = strings(&["  dist/** ", "", "   ", "a/**", "dist/**", "a/**"]);
        assert_eq!(
            normalize_patterns(&raw).unwrap(),
            strings(&["dist/**", "a/**"])
        );
    }

    #[test]
    fn normalize_strips_leading_slash() {
        let raw = strings(&["/dist/**", "dist/**", " /a/** ", "/"]);
        assert_eq!(
            normalize_patterns(&raw).unwrap(),
            strings(&["dist/**", "a/**"])
        );
        let matcher = IgnoreMatcher::new(&normalize_patterns(&raw).unwrap()).unwrap();
        assert!(matcher.is_ignored("dist/app.js"));
    }

    #[test]
    fn normalize_limits() {
        assert_eq!(
            normalize_patterns(&["a".repeat(501)]),
            Err(IgnoreError::TooLong("a".repeat(501)))
        );
        assert!(normalize_patterns(&["a".repeat(500)]).is_ok());
        let many: Vec<String> = (0..5001).map(|i| format!("dir{i}/**")).collect();
        assert_eq!(normalize_patterns(&many), Err(IgnoreError::TooMany));
        assert_eq!(normalize_patterns(&many[..5000]).unwrap().len(), 5000);
    }

    #[test]
    fn matcher_rejects_list_too_complex_to_compile() {
        // Tiap pola lolos validasi, tetapi GlobSet gabungannya melewati batas ukuran NFA.
        let patterns: Vec<String> = (0..300)
            .map(|i| format!("{}{i}", "?".repeat(490)))
            .collect();
        assert!(normalize_patterns(&patterns).is_ok());
        assert_eq!(
            IgnoreMatcher::new(&patterns).err(),
            Some(IgnoreError::TooComplex)
        );
    }

    #[test]
    fn error_messages_name_the_pattern() {
        let err = normalize_patterns(&strings(&["a[b"])).unwrap_err();
        assert!(err.to_string().contains("a[b"));
    }

    #[test]
    fn title_matches_case_insensitive() {
        assert!(title_matches("WIP: x", &strings(&["wip"])));
        assert!(!title_matches("WIP: x", &strings(&[""])));
        assert!(!title_matches("feat: x", &strings(&["wip"])));
    }
}
