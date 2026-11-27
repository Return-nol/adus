//! Conversations in Adus review threads (`Job::ThreadReply`).

use uuid::Uuid;

use crate::engine::{Engine, EngineError};

impl Engine {
    /// Answer a human reply in a review thread. Filled in by the thread-conversation engine.
    pub async fn thread_reply(
        &self,
        _repository_id: Uuid,
        _pr_number: i64,
        _thread_id: &str,
        _comment_id: &str,
    ) -> Result<(), EngineError> {
        Ok(())
    }
}
