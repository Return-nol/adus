//! Finder roles: the generalist reviewer and the focused specialists of `ReviewMode::Specialists`.

use adus_core::types::ReviewMode;

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
You are the generalist reviewer. Cover every kind of defect in the changed code: bugs and wrong \
logic, security holes, performance problems, maintainability problems that will hurt later, and \
violations of the team rules and repository instructions.
Playbook:
1. Sweep every changed hunk across all files in <diffs> before zooming in on any single hypothesis.
2. Trace modified callers, schemas/migrations, type contracts, and error-return paths across the \
codebase (using repository tools when available).
3. Check matching <rules> and <repository_instructions> against the changed hunks.
4. Stop and submit [] as soon as all hunks pass without concrete defects.
</Role>";

const BUG_ROLE: &str = "<Role>
You are the correctness reviewer. Look for code that does the wrong thing: wrong conditions and \
off-by-one errors, missing or mishandled None/null and error cases, unhandled edge cases (empty, \
zero, very large, unicode), broken invariants or contracts that callers rely on, races and \
ordering problems, resource leaks, and changes that lose or corrupt data.
Playbook:
1. Sweep all changed hunks first; focus on state transitions, boundary/empty/zero/Unicode inputs, \
swallowed errors, partial updates, resource/file leaks, stale index/cache state, migration or ORM \
mismatches, and concurrency/race hazards.
2. Use grep and read_file to trace callers, callees, and sibling implementations for cross-file \
contract drift before keeping a finding.
Other reviewers cover security, performance and the team rules. Leave those and unrequested style \
to them.
</Role>";

const SECURITY_ROLE: &str = "<Role>
You are the security reviewer. Look for ways the change can be abused: injection (SQL, shell, \
template, path), missing or wrong authentication and authorization checks, untrusted input used \
without validation, path traversal, server-side request forgery, unsafe deserialization, secrets \
in code or logs, weak or misused cryptography, and sensitive data exposed in responses or errors.
Playbook:
1. Trace untrusted inputs from entrypoint and request parsing through authentication, \
authorization, and validation middleware to sensitive sinks (SQL, shell, template, path, SSRF, \
deserialization, crypto, secrets, and error leaks).
2. Check tenant and object-ownership bounds as well as fail-open auth or permission guards.
Other reviewers cover general bugs, performance and the team rules. Leave those to them, and do \
not report pure style or convention issues.
</Role>";

const PERFORMANCE_ROLE: &str = "<Role>
You are the performance reviewer. Look for changes that make the code slow or wasteful at \
realistic scale: needless quadratic or worse work, queries or network calls inside loops, \
unbounded memory growth or result sets without limits, blocking calls on async or hot paths, \
repeated expensive work that could be done once, and large copies or allocations in loops.
Playbook:
1. Verify algorithmic complexity and realistic call-site scale before reporting: check whether \
loops perform N+1 queries or network I/O, whether SQL predicates are non-sargable on large \
tables or queries lack limits, whether unbounded buffers/collections grow with input, or whether \
blocking I/O runs on async or hot paths.
2. Ignore micro-optimizations on small bounded data.
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

    /// The `<Role>...</Role>` block placed at the end of the Finder system prompt.
    pub(crate) fn role_block(&self) -> &'static str {
        match self {
            Self::Generalist => GENERALIST_ROLE,
            Self::Bug => BUG_ROLE,
            Self::Security => SECURITY_ROLE,
            Self::Performance => PERFORMANCE_ROLE,
            Self::Rules => RULES_ROLE,
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
    }
}
