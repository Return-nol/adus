//! Job yang dibawa antrian dari webhook ke worker.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::ReviewMode;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Job {
    ReviewPr {
        repository_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
        /// Forces a review mode for this run (`@adudu review deep`); `None` = configured mode.
        /// `default` keeps jobs queued before this field existed readable.
        #[serde(default)]
        mode_override: Option<ReviewMode>,
    },
    PrComment {
        repository_id: Uuid,
        pr_number: i64,
        comment_url: Option<String>,
        author: CommentAuthor,
        body: String,
    },
    /// Learn by-design exceptions from the discussion of a closed PR.
    LearnPr { repository_id: Uuid, pr_number: i64 },
    /// A human reply in a review thread. Carries no body: the engine re-reads the thread and
    /// decides whether it is an Adus thread.
    ThreadReply {
        repository_id: Uuid,
        pr_number: i64,
        /// Provider thread id: GitHub root review comment id, Azure thread id.
        thread_id: String,
        comment_id: String,
    },
}

/// Apa yang memicu review PR. Juga disimpan di `reviews.trigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "review_trigger", rename_all = "snake_case")]
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
            mode_override: None,
        };
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "review_pr",
                "repository_id": "00000000-0000-0000-0000-000000000000",
                "pr_number": 42,
                "trigger": "opened",
                "mode_override": null,
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

        let learn = Job::LearnPr {
            repository_id: Uuid::nil(),
            pr_number: 9,
        };
        let json = serde_json::to_value(&learn).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "learn_pr",
                "repository_id": "00000000-0000-0000-0000-000000000000",
                "pr_number": 9,
            })
        );
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), learn);

        let reply = Job::ThreadReply {
            repository_id: Uuid::nil(),
            pr_number: 3,
            thread_id: "284312630".to_string(),
            comment_id: "284312631".to_string(),
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "thread_reply",
                "repository_id": "00000000-0000-0000-0000-000000000000",
                "pr_number": 3,
                "thread_id": "284312630",
                "comment_id": "284312631",
            })
        );
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), reply);
    }

    #[test]
    fn job_review_pr_without_mode_override_deserializes_none() {
        // Jobs queued before `mode_override` existed must still load.
        let legacy = r#"{"type":"review_pr","repository_id":"00000000-0000-0000-0000-000000000000","pr_number":1,"trigger":"updated"}"#;
        assert_eq!(
            serde_json::from_str::<Job>(legacy).unwrap(),
            Job::ReviewPr {
                repository_id: Uuid::nil(),
                pr_number: 1,
                trigger: Trigger::Updated,
                mode_override: None,
            }
        );
    }

    #[test]
    fn job_review_pr_mode_override_roundtrip() {
        let job = Job::ReviewPr {
            repository_id: Uuid::nil(),
            pr_number: 1,
            trigger: Trigger::Command,
            mode_override: Some(ReviewMode::Specialists),
        };
        let json = serde_json::to_string(&job).unwrap();
        assert!(json.contains(r#""mode_override":"specialists""#));
        assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), job);
    }
}
