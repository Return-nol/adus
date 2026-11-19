use crate::error::ConfigError;
use std::collections::HashMap;

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
