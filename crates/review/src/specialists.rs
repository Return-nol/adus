//! Finder roles: the generalist reviewer and the focused specialists of `ReviewMode::Specialists`.
//! Team rules are checked by the Sharded Rules (`rules_judge.rs`) whenever the repository tools
//! are available; only diff-only Finder runs check them, in the Generalist or Bug role.

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
}

/// The three lenses of the Generalist role, shared by its tool-enabled and diff-only blocks.
macro_rules! generalist_lenses {
    () => {
        "Bug lens:
- swallowed errors: an error logged and dropped, replaced by a default value, or never returned;
- an error treated as a miss: a failed lookup read as a cache miss or not-found, so the caller \
carries on with missing data;
- a dropped fallback: a default, retry or recovery path the old code had and the new code lost; \
for every branch the change adds or reroutes (a new mode, location, provider or flag value), \
check what happens when that branch's own dependency is missing, disabled or fails, and whether \
it still reaches a correct fallback instead of a generic default;
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
"
    };
}

/// The correctness checklist of the Bug role, shared by its tool-enabled and diff-only blocks.
macro_rules! bug_checklist {
    () => {
        "You are the correctness reviewer. Challenge every changed function for code that does the wrong \
thing:
- swallowed errors, errors treated as a cache miss or not-found, dropped fallbacks and defaults;
- for every branch the change adds or reroutes (a new mode, location, provider or flag value): \
what happens when that branch's own dependency is missing, disabled or fails, and whether it \
still reaches a correct fallback instead of a generic default;
- wrong conditions, off-by-one errors, missing None/null cases, empty/zero/very large/unicode \
inputs, SQL NULL propagation (CONCAT, outer joins, NOT IN);
- broken invariants or contracts that callers rely on, wrappers that drop or swap an argument;
- races and check-then-act windows, partial updates, resource leaks, stale cache or index state, \
migrations or ORM mappings that disagree with the code.
"
    };
}

const GENERALIST_ROLE: &str = concat!(
    "<Role>
You are the generalist reviewer. Challenge every changed function through all three lenses below, \
then check the changed hunks against <repository_instructions>.
",
    generalist_lenses!(),
    "Give each finding the \"category\" of the lens it belongs to, or \"repo_instruction\" when it \
breaks a repository instruction. The team rules are checked separately; leave them out.
</Role>"
);

const GENERALIST_ROLE_DIFF_ONLY: &str = concat!(
    "<Role>
You are the generalist reviewer. Challenge every changed function through all three lenses below, \
then check the changed hunks against the matching <rules> and <repository_instructions>.
",
    generalist_lenses!(),
    "Give each finding the \"category\" of the lens it belongs to, or \"repo_instruction\" when it \
breaks a team rule or repository instruction.
</Role>"
);

const BUG_ROLE: &str = concat!(
    "<Role>\n",
    bug_checklist!(),
    "Use grep and read_file on callers, callees and sibling implementations to find contract drift \
before promoting or dismissing a suspicion.
Then check the changed hunks against <repository_instructions>, and give a finding that breaks \
one the category \"repo_instruction\".
Other reviewers cover security, performance and the team rules. Leave those, and style no \
repository instruction asks for, to them.
</Role>"
);

const BUG_ROLE_DIFF_ONLY: &str = concat!(
    "<Role>\n",
    bug_checklist!(),
    "Compare the callers, callees and sibling implementations visible in the diff for contract drift \
before promoting or dismissing a suspicion.
Then check the changed hunks against the matching <rules> and <repository_instructions>, and give \
a finding that breaks one the category \"repo_instruction\".
Other reviewers cover security and performance. Leave those, and style no rule or instruction \
asks for, to them.
</Role>"
);

const SECURITY_ROLE: &str = "<Role>
You are the security reviewer. Challenge every changed function as an attacker would:
- follow untrusted input from where it enters (request parsing, headers, files, queue messages) \
through authentication, authorization and validation to every sensitive sink: SQL, shell, \
template, path, URL fetches (SSRF), deserialization;
- check tenant and object-ownership bounds, and whether an auth or permission guard fails open on \
an error or a missing value;
- look for secrets in code or logs, weak or misused cryptography, and sensitive data exposed in \
responses or error messages.
<repository_instructions> is context only: the correctness reviewer checks the code against it.
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
<repository_instructions> is context only: the correctness reviewer checks the code against it.
Other reviewers cover general bugs, security and the team rules. Leave those to them, and do \
not report pure style or convention issues.
</Role>";

impl SpecialistKind {
    /// Name used in summary notes, e.g. `"Performance reviewer did not complete"`.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Generalist => "Generalist",
            Self::Bug => "Bug",
            Self::Security => "Security",
            Self::Performance => "Performance",
        }
    }

    /// The category of a finding this Finder reports without one. `None` for the generalist,
    /// which covers every category and must name it. Bug names `repo_instruction` itself for a
    /// finding that breaks a repository instruction or (diff-only) a team rule.
    pub(crate) fn category(&self) -> Option<FindingCategory> {
        match self {
            Self::Generalist => None,
            Self::Bug => Some(FindingCategory::Bug),
            Self::Security => Some(FindingCategory::Security),
            Self::Performance => Some(FindingCategory::Performance),
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
            (Self::Generalist, true) => GENERALIST_ROLE,
            (Self::Generalist, false) => GENERALIST_ROLE_DIFF_ONLY,
            (Self::Bug, true) => BUG_ROLE,
            (Self::Bug, false) => BUG_ROLE_DIFF_ONLY,
            (Self::Security, _) => SECURITY_ROLE,
            (Self::Performance, _) => PERFORMANCE_ROLE,
        }
    }

    /// Whether a diff-only run of this Finder is given the matching team rules to check. With
    /// the repository tools no Finder is: the Sharded Rules check them.
    pub(crate) fn checks_rules_diff_only(&self) -> bool {
        matches!(self, Self::Generalist | Self::Bug)
    }
}

/// The Finder agents that review one batch, in a fixed order.
pub(crate) fn active_specialists(mode: ReviewMode) -> Vec<SpecialistKind> {
    match mode {
        ReviewMode::Generalist => vec![SpecialistKind::Generalist],
        ReviewMode::Specialists => vec![
            SpecialistKind::Bug,
            SpecialistKind::Security,
            SpecialistKind::Performance,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SpecialistKind::*;

    #[test]
    fn specialists_mode_runs_bug_security_performance_and_bug_checks_repository_instructions() {
        assert_eq!(
            active_specialists(ReviewMode::Specialists),
            vec![Bug, Security, Performance]
        );
        assert_eq!(active_specialists(ReviewMode::Generalist), vec![Generalist]);
        assert!(Bug.role_block().contains("grep"));
        assert!(Security.role_block().contains("validation"));
        assert!(Performance.role_block().contains("scale"));
        assert!(!Bug.role_block_for(false).contains("grep"));
        for with_tools in [true, false] {
            let bug = Bug.role_block_for(with_tools);
            assert!(bug.contains("promoting or dismissing"));
            assert!(bug.contains("swallowed errors"));
            // Bug and Generalist check the changed hunks against the repository instructions,
            // and against the team rules only when no Sharded Rules run (diff-only).
            for kind in [Generalist, Bug] {
                let role = kind.role_block_for(with_tools);
                assert!(
                    role.contains("check the changed hunks against"),
                    "{kind:?} {with_tools}"
                );
                assert!(role.contains("<repository_instructions>"), "{kind:?}");
                assert!(role.contains("\"repo_instruction\""), "{kind:?}");
                assert_eq!(role.contains("<rules>"), !with_tools, "{kind:?}");
            }
            // Security and Performance read the instructions as context only.
            for kind in [Security, Performance] {
                let role = kind.role_block_for(with_tools);
                assert!(
                    role.contains("<repository_instructions> is context only"),
                    "{kind:?}"
                );
                assert!(!role.contains("repo_instruction\""), "{kind:?}");
                assert!(!role.contains("<rules>"), "{kind:?}");
            }
        }
        assert_eq!(Generalist.category(), None);
        assert_eq!(Bug.category(), Some(FindingCategory::Bug));
    }
}
