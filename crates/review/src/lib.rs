pub mod agent;
pub mod command;
pub mod dedup;
pub mod engine;
pub mod finder;
pub mod parse;
pub mod prompt;
pub mod repo_instructions;
pub mod specialists;
pub mod tools;
pub mod verifier;
pub mod workspace;

pub use dedup::{DedupResult, deduplicate_findings};
pub use parse::{
    ParseError, Suggestion, keep_valid, keep_valid_batch, parse_submit_findings, parse_suggestions,
    submit_findings_tool_def,
};
pub use prompt::{
    DiffBatch, MARKER, MAX_BATCH_CHARS, ManifestEntry, batch_targets, build_finder_system_prompt,
    build_finder_user_prompt, build_manifest, build_prompt, format_inline, format_summary,
};
pub use specialists::{SpecialistKind, active_specialists};
pub use verifier::{
    CandidateFinding, MAX_VERIFY_CANDIDATES, VERIFY_CONCURRENCY, VERIFY_MAX_STEPS,
    VerificationReport, VerifyContext, verify_candidates,
};
