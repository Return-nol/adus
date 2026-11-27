mod agent;
mod budget;
pub mod command;
mod coverage;
mod dedup;
mod discussion;
pub mod engine;
mod enrich;
pub mod finder;
mod learn;
mod parse;
mod prompt;
mod redact;
mod repo_instructions;
mod rules_evidence;
mod rules_judge;
mod severity;
mod small_pr;
mod specialists;
mod threads;
mod tools;
mod trace;
mod verifier;
pub mod workspace;

#[cfg(test)]
pub(crate) mod test_support;
