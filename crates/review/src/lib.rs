pub mod agent;
pub mod command;
pub mod engine;
pub mod parse;
pub mod prompt;
pub mod repo_instructions;
pub mod specialists;
pub mod tools;
pub mod workspace;

pub use parse::{
    ParseError, Suggestion, keep_valid, keep_valid_batch, parse_submit_findings, parse_suggestions,
    submit_findings_tool_def,
};
pub use prompt::{
    DiffBatch, MARKER, MAX_BATCH_CHARS, ManifestEntry, batch_targets, build_finder_system_prompt,
    build_finder_user_prompt, build_manifest, build_prompt, format_inline, format_summary,
};
pub use specialists::{SpecialistKind, active_specialists};
