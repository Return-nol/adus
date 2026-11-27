//! Shared unit-test fakes and fixture builders for `adus-review`.

use std::collections::VecDeque;
use std::fs;
use std::sync::Mutex;
use std::time::Duration;

use adus_core::types::Severity;
use adus_db::memories::{Memory, MemoryKind, MemorySource, MemoryStatus};
use adus_db::rules::{KnowledgeScope, Rule};
use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{ChatRequest, ChatResponse, Message, StopReason, ToolCall, UserPart};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

use crate::workspace::RepoWorkspace;

impl RepoWorkspace {
    pub(crate) fn from_files(files: &[(&str, &[u8])]) -> Self {
        let dir = TempDir::new().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        Self::from_temp_dir(dir).unwrap()
    }
}

/// Scripted `ChatService` returning responses in order and recording requests/calls.
#[derive(Default)]
pub(crate) struct ScriptedChat {
    script: Mutex<VecDeque<Result<ChatResponse, ChatError>>>,
    latencies: Mutex<VecDeque<Duration>>,
    requests: Mutex<Vec<ChatRequest>>,
    calls: Mutex<Vec<(Uuid, UsageContext)>>,
}

impl ScriptedChat {
    pub(crate) fn new(script: Vec<Result<ChatResponse, ChatError>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            ..Self::default()
        }
    }

    pub(crate) fn with_latencies(
        script: Vec<Result<ChatResponse, ChatError>>,
        latencies: Vec<Duration>,
    ) -> Self {
        Self {
            script: Mutex::new(script.into()),
            latencies: Mutex::new(latencies.into()),
            ..Self::default()
        }
    }

    pub(crate) fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn calls(&self) -> Vec<(Uuid, UsageContext)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatService for ScriptedChat {
    async fn chat(
        &self,
        provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        self.requests.lock().unwrap().push(req);
        self.calls.lock().unwrap().push((provider_id, ctx));
        let latency = self.latencies.lock().unwrap().pop_front();
        if let Some(latency) = latency {
            tokio::time::sleep(latency).await;
        }
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ChatError::Invalid("script exhausted".into())))
    }
}

pub(crate) enum Reply {
    Answer(Result<ChatResponse, ChatError>),
    Hang,
}

pub(crate) fn answer(result: Result<ChatResponse, ChatError>) -> Reply {
    Reply::Answer(result)
}

/// Fake `ChatService` routing replies by candidate title in the first user message.
#[derive(Default)]
pub(crate) struct RoutedChat {
    routes: Mutex<Vec<(String, VecDeque<Reply>)>>,
    keep_by_default: bool,
    requests: Mutex<Vec<ChatRequest>>,
    calls: Mutex<Vec<(Uuid, UsageContext)>>,
}

impl RoutedChat {
    pub(crate) fn new(routes: Vec<(&str, Vec<Reply>)>) -> Self {
        Self {
            routes: Mutex::new(
                routes
                    .into_iter()
                    .map(|(title, replies)| (title.to_string(), replies.into()))
                    .collect(),
            ),
            ..Self::default()
        }
    }

    pub(crate) fn keeping_everything() -> Self {
        Self {
            keep_by_default: true,
            ..Self::default()
        }
    }

    pub(crate) fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn requests_for(&self, title: &str) -> Vec<ChatRequest> {
        self.requests()
            .into_iter()
            .filter(|req| is_for(req, title))
            .collect()
    }

    pub(crate) fn calls(&self) -> Vec<(Uuid, UsageContext)> {
        self.calls.lock().unwrap().clone()
    }
}

pub(crate) fn is_for(req: &ChatRequest, title: &str) -> bool {
    user_text(req).contains(&format!("\"title\": \"{title}\""))
}

#[async_trait]
impl ChatService for RoutedChat {
    async fn chat(
        &self,
        provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        let reply = {
            let mut routes = self.routes.lock().unwrap();
            let route = routes.iter_mut().find(|(title, _)| is_for(&req, title));
            match route.and_then(|(_, replies)| replies.pop_front()) {
                Some(reply) => reply,
                None if self.keep_by_default => Reply::Answer(text_turn(r#"{"keep": true}"#)),
                None => Reply::Answer(Err(ChatError::Invalid("script exhausted".into()))),
            }
        };
        self.requests.lock().unwrap().push(req);
        self.calls.lock().unwrap().push((provider_id, ctx));
        match reply {
            Reply::Answer(answer) => answer,
            Reply::Hang => std::future::pending().await,
        }
    }
}

pub(crate) fn text_turn(text: &str) -> Result<ChatResponse, ChatError> {
    Ok(ChatResponse {
        text: text.into(),
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
        input_tokens: 100,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        provider_meta: None,
    })
}

pub(crate) fn tools_turn(calls: Vec<ToolCall>) -> Result<ChatResponse, ChatError> {
    Ok(ChatResponse {
        text: String::new(),
        tool_calls: calls,
        stop_reason: StopReason::ToolUse,
        input_tokens: 100,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        provider_meta: None,
    })
}

pub(crate) fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        provider_meta: None,
    }
}

pub(crate) fn msg_text(msg: &Message) -> Option<&str> {
    match msg {
        Message::User { parts } => parts.first().map(|UserPart::Text(t)| t.as_str()),
        _ => None,
    }
}

pub(crate) fn user_text(req: &ChatRequest) -> &str {
    req.messages.first().and_then(msg_text).unwrap_or("")
}

pub(crate) fn tool_names(req: &ChatRequest) -> Vec<&str> {
    req.tools.iter().map(|t| t.name.as_str()).collect()
}

pub(crate) fn rule(title: &str, severity: Severity, glob: Option<&str>, enabled: bool) -> Rule {
    Rule {
        id: Uuid::nil(),
        scope: KnowledgeScope::Global,
        repository_id: None,
        title: title.into(),
        body: format!("body of {title}"),
        path_glob: glob.map(str::to_string),
        severity,
        enabled,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

pub(crate) fn memory(content: &str) -> Memory {
    Memory {
        id: Uuid::nil(),
        scope: KnowledgeScope::Global,
        repository_id: None,
        content: content.into(),
        source: MemorySource::Manual,
        source_url: None,
        created_by: None,
        created_by_email: None,
        enabled: true,
        status: MemoryStatus::Active,
        kind: MemoryKind::Convention,
        context: None,
        reason: None,
        resolution: None,
        evidence: Vec::new(),
        supersedes: None,
        superseded_by: None,
        created_at: Utc::now(),
    }
}
