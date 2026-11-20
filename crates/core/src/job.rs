//! Job yang dibawa antrian dari webhook ke worker.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Job {
    ReviewPr {
        repository_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
    },
    PrComment {
        repository_id: Uuid,
        pr_number: i64,
        comment_url: Option<String>,
        author: CommentAuthor,
        body: String,
    },
}

/// Apa yang memicu review PR.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Opened,
    Updated,
    Command,
}

/// Penulis komentar PR, sebagaimana dilaporkan provider git.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentAuthor {
    pub login: Option<String>,
    pub email: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_json_roundtrip() {
        let review = Job::ReviewPr {
            repository_id: Uuid::nil(),
            pr_number: 42,
            trigger: Trigger::Opened,
        };
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "review_pr",
                "repository_id": "00000000-0000-0000-0000-000000000000",
                "pr_number": 42,
                "trigger": "opened",
            })
        );
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), review);

        let comment = Job::PrComment {
            repository_id: Uuid::nil(),
            pr_number: 7,
            comment_url: Some("https://example.test/c/1".to_string()),
            author: CommentAuthor {
                login: Some("octocat".to_string()),
                email: None,
            },
            body: "@adudu review".to_string(),
        };
        let json = serde_json::to_string(&comment).unwrap();
        assert!(json.contains(r#""type":"pr_comment""#));
        assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), comment);
    }
}
