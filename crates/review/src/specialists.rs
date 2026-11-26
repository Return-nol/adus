//! Finder roles: the generalist reviewer and the focused specialists of `ReviewMode::Specialists`.

use adus_core::types::ReviewMode;

use crate::parse::FindingCategory;

/// Which Finder agent reviews a batch. Its `<Role>` block ends the Finder system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SpecialistKind {
    /// The only Finder in `ReviewMode::Generalist`: every kind of defect.
    Generalist,
    /// Correctness: logic errors, edge cases, error handling, concurrency.
    Bug,
    Security,
    Performance,
    /// Team rules and repository instructions only. Runs when a rule matches the batch or repository instructions are loaded.
    Rules,
}

const GENERALIST_ROLE: &str = "<Role>
You are the generalist reviewer. Challenge every changed function through all three lenses below, \
then check the matching <rules> and <repository_instructions> against the changed hunks.
Bug lens:
- swallowed errors: an error logged and dropped, replaced by a default value, or never returned;
- an error treated as a miss: a failed lookup read as a cache miss or not-found, so the caller \
carries on with missing data;
- a dropped fallback: a default, retry or recovery path the old code had and the new code lost;
- SQL NULL propagation: NULL in CONCAT or arithmetic yields NULL, outer joins add NULL columns, \
NOT IN over a NULL matches nothing;
- wrong conditions and off-by-one errors, empty/zero/very large/unicode inputs, broken invariants \
that callers rely on;
- races and check-then-act (TOCTOU) windows, partial updates outside a transaction;
- migration hazards: locking or rewriting large tables, dropping data, columns the code and the \
schema disagree on;
- wrappers and delegations that drop or swap an argument or ignore the inner result.
Security lens:
- untrusted input reaching SQL, shell, template, path or URL sinks without validation;
- missing or wrong authentication, authorization, tenant or ownership checks, and guards that \
fail open;
- path traversal, server-side request forgery, unsafe deserialization, secrets in code or logs, \
sensitive data in responses or errors.
Performance lens:
- queries or network calls inside loops, queries without a limit, loops and collections that \
grow without bound with the input;
- blocking calls on async or hot paths, expensive work repeated where it could run once.
Give each finding the \"category\" of the lens it belongs to, or \"repo_instruction\" when it \
breaks a team rule or repository instruction.
</Role>";

const BUG_ROLE: &str = "<Role>
You are the correctness reviewer. Challenge every changed function for code that does the wrong \
thing:
- swallowed errors, errors treated as a cache miss or not-found, dropped fallbacks and defaults;
- wrong conditions, off-by-one errors, missing None/null cases, empty/zero/very large/unicode \
inputs, SQL NULL propagation (CONCAT, outer joins, NOT IN);
- broken invariants or contracts that callers rely on, wrappers that drop or swap an argument;
- races and check-then-act windows, partial updates, resource leaks, stale cache or index state, \
migrations or ORM mappings that disagree with the code.
Use grep and read_file on callers, callees and sibling implementations to find contract drift \
before promoting or dismissing a suspicion.
Other reviewers cover security, performance and the team rules. Leave those and unrequested style \
to them.
</Role>";

const BUG_ROLE_DIFF_ONLY: &str = "<Role>
You are the correctness reviewer. Challenge every changed function for code that does the wrong \
thing:
- swallowed errors, errors treated as a cache miss or not-found, dropped fallbacks and defaults;
- wrong conditions, off-by-one errors, missing None/null cases, empty/zero/very large/unicode \
inputs, SQL NULL propagation (CONCAT, outer joins, NOT IN);
- broken invariants or contracts that callers rely on, wrappers that drop or swap an argument;
- races and check-then-act windows, partial updates, resource leaks, stale cache or index state, \
migrations or ORM mappings that disagree with the code.
Compare the callers, callees and sibling implementations visible in the diff for contract drift \
before promoting or dismissing a suspicion.
Other reviewers cover security, performance and the team rules. Leave those and unrequested style \
to them.
</Role>";

const SECURITY_ROLE: &str = "<Role>
You are the security reviewer. Challenge every changed function as an attacker would:
- follow untrusted input from where it enters (request parsing, headers, files, queue messages) \
through authentication, authorization and validation to every sensitive sink: SQL, shell, \
template, path, URL fetches (SSRF), deserialization;
- check tenant and object-ownership bounds, and whether an auth or permission guard fails open on \
an error or a missing value;
- look for secrets in code or logs, weak or misused cryptography, and sensitive data exposed in \
responses or error messages.
Other reviewers cover general bugs, performance and the team rules. Leave those to them, and do \
not report pure style or convention issues.
</Role>";

const PERFORMANCE_ROLE: &str = "<Role>
You are the performance reviewer. Challenge every changed function at the scale it really runs at:
- queries or network calls inside loops (N+1), queries and result sets without a limit, \
predicates that cannot use an index on large tables;
- quadratic or worse work, memory or collections that grow without bound with the input, large \
copies or allocations inside loops;
- blocking calls on async or hot paths, and expensive work repeated where it could run once.
Establish how large the input and how hot the call site really are before reporting; small, \
bounded data is not a finding.
Other reviewers cover general bugs, security and the team rules. Leave those to them, and do \
not report pure style or convention issues.
</Role>";

const RULES_ROLE: &str = "<Role>
You are the team-rules reviewer. Check the changed code only against the numbered team rules in \
<rules> and the repository instructions in <repository_instructions>.
Playbook:
1. Check every changed hunk strictly against matching <rules> and <repository_instructions>.
2. Verify local patterns and documented exemptions in the codebase before flagging a violation.
3. Cite the exact rule or instruction in the body and use the rule's severity. Style and \
convention issues are in scope when a rule or instruction asks for them.
Other reviewers cover bugs, security and performance. Do not report anything that no rule or \
instruction asks for.
</Role>";

impl SpecialistKind {
    /// Name used in summary notes, e.g. `"Performance reviewer did not complete"`.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Generalist => "Generalist",
            Self::Bug => "Bug",
            Self::Security => "Security",
            Self::Performance => "Performance",
            Self::Rules => "Rules",
        }
    }

    /// The category of a finding this Finder reports without one. `None` for the generalist,
    /// which covers every category and must name it.
    pub(crate) fn category(&self) -> Option<FindingCategory> {
        match self {
            Self::Generalist => None,
            Self::Bug => Some(FindingCategory::Bug),
            Self::Security => Some(FindingCategory::Security),
            Self::Performance => Some(FindingCategory::Performance),
            Self::Rules => Some(FindingCategory::RepoInstruction),
        }
    }

    /// The `<Role>...</Role>` block placed at the end of the Finder system prompt (tool-enabled).
    #[cfg(test)]
    pub(crate) fn role_block(&self) -> &'static str {
        self.role_block_for(true)
    }

    /// The `<Role>...</Role>` block for either tool-enabled (`with_tools = true`) or diff-only
    /// (`with_tools = false`) Finder runs.
    pub(crate) fn role_block_for(&self, with_tools: bool) -> &'static str {
        match (self, with_tools) {
            (Self::Generalist, _) => GENERALIST_ROLE,
            (Self::Bug, true) => BUG_ROLE,
            (Self::Bug, false) => BUG_ROLE_DIFF_ONLY,
            (Self::Security, _) => SECURITY_ROLE,
            (Self::Performance, _) => PERFORMANCE_ROLE,
            (Self::Rules, _) => RULES_ROLE,
        }
    }
}

/// The Finder agents that review one batch, in a fixed order.
pub(crate) fn active_specialists(
    mode: ReviewMode,
    has_rules_or_instructions: bool,
) -> Vec<SpecialistKind> {
    match mode {
        ReviewMode::Generalist => vec![SpecialistKind::Generalist],
        ReviewMode::Specialists => {
            let mut kinds = vec![
                SpecialistKind::Bug,
                SpecialistKind::Security,
                SpecialistKind::Performance,
            ];
            if has_rules_or_instructions {
                kinds.push(SpecialistKind::Rules);
            }
            kinds
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SpecialistKind::*;

    #[test]
    fn active_specialists_omits_rules_when_none_match() {
        assert_eq!(
            active_specialists(ReviewMode::Specialists, false),
            vec![Bug, Security, Performance]
        );
        assert_eq!(
            active_specialists(ReviewMode::Specialists, true),
            vec![Bug, Security, Performance, Rules]
        );
        assert_eq!(
            active_specialists(ReviewMode::Generalist, true),
            vec![Generalist]
        );
        assert_eq!(
            active_specialists(ReviewMode::Generalist, false),
            vec![Generalist]
        );
        assert!(SpecialistKind::Bug.role_block().contains("grep"));
        assert!(SpecialistKind::Security.role_block().contains("validation"));
        assert!(SpecialistKind::Performance.role_block().contains("scale"));
        assert!(
            SpecialistKind::Rules
                .role_block()
                .contains("<repository_instructions>")
        );
        assert!(!SpecialistKind::Bug.role_block_for(false).contains("grep"));
        for with_tools in [true, false] {
            let bug = SpecialistKind::Bug.role_block_for(with_tools);
            assert!(bug.contains("promoting or dismissing"));
            assert!(bug.contains("swallowed errors"));
        }
        assert_eq!(Generalist.category(), None);
        assert_eq!(Rules.category(), Some(FindingCategory::RepoInstruction));
    }
}
