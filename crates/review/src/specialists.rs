//! Finder roles: the generalist reviewer and the focused specialists of `ReviewMode::Specialists`.

use adus_core::types::ReviewMode;

/// Which Finder agent reviews a batch. Its `<Role>` block ends the Finder system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialistKind {
    /// The only Finder in `ReviewMode::Generalist`: every kind of defect.
    Generalist,
    /// Correctness: logic errors, edge cases, error handling, concurrency.
    Bug,
    Security,
    Performance,
    /// Team rules and repository instructions only. Runs only when a rule matches the batch.
    Rules,
}

const GENERALIST_ROLE: &str = "<Role>
You are the generalist reviewer. Cover every kind of defect in the changed code: bugs and wrong \
logic, security holes, performance problems, maintainability problems that will hurt later, and \
violations of the team rules and repository instructions.
</Role>";

const BUG_ROLE: &str = "<Role>
You are the correctness reviewer. Look for code that does the wrong thing: wrong conditions and \
off-by-one errors, missing or mishandled None/null and error cases, unhandled edge cases (empty, \
zero, very large, unicode), broken invariants or contracts that callers rely on, races and \
ordering problems, resource leaks, and changes that lose or corrupt data.
Other reviewers cover security, performance and the team rules. Leave those to them, and do not \
report pure style or convention issues.
</Role>";

const SECURITY_ROLE: &str = "<Role>
You are the security reviewer. Look for ways the change can be abused: injection (SQL, shell, \
template, path), missing or wrong authentication and authorization checks, untrusted input used \
without validation, path traversal, server-side request forgery, unsafe deserialization, secrets \
in code or logs, weak or misused cryptography, and sensitive data exposed in responses or errors.
Other reviewers cover general bugs, performance and the team rules. Leave those to them, and do \
not report pure style or convention issues.
</Role>";

const PERFORMANCE_ROLE: &str = "<Role>
You are the performance reviewer. Look for changes that make the code slow or wasteful at \
realistic scale: needless quadratic or worse work, queries or network calls inside loops, \
unbounded memory growth or result sets without limits, blocking calls on async or hot paths, \
repeated expensive work that could be done once, and large copies or allocations in loops.
Report only problems with a real cost, not micro-optimizations. Other reviewers cover general \
bugs, security and the team rules. Leave those to them, and do not report pure style or \
convention issues.
</Role>";

const RULES_ROLE: &str = "<Role>
You are the team-rules reviewer. Check the changed code only against the numbered team rules in \
<rules> and the repository instructions in <repository_instructions>. Report a finding only when \
a changed line breaks a specific rule or instruction that applies to its file; name that rule or \
instruction in the body and use the rule's severity. Style and convention issues are in scope \
when a rule or instruction asks for them.
Other reviewers cover bugs, security and performance. Do not report anything that no rule or \
instruction asks for.
</Role>";

impl SpecialistKind {
    /// Name used in summary notes, e.g. `"Performance reviewer did not complete"`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Generalist => "Generalist",
            Self::Bug => "Bug",
            Self::Security => "Security",
            Self::Performance => "Performance",
            Self::Rules => "Rules",
        }
    }

    /// The `<Role>...</Role>` block placed at the end of the Finder system prompt.
    pub fn role_block(&self) -> &'static str {
        match self {
            Self::Generalist => GENERALIST_ROLE,
            Self::Bug => BUG_ROLE,
            Self::Security => SECURITY_ROLE,
            Self::Performance => PERFORMANCE_ROLE,
            Self::Rules => RULES_ROLE,
        }
    }

    /// Whether this role leaves pure style and convention issues out entirely (even those a team
    /// rule asks for, which the `Rules` specialist covers).
    pub fn excludes_pure_style(&self) -> bool {
        matches!(self, Self::Bug | Self::Security | Self::Performance)
    }
}

/// The Finder agents that review one batch, in a fixed order.
pub fn active_specialists(mode: ReviewMode, batch_has_rules: bool) -> Vec<SpecialistKind> {
    match mode {
        ReviewMode::Generalist => vec![SpecialistKind::Generalist],
        ReviewMode::Specialists => {
            let mut kinds = vec![
                SpecialistKind::Bug,
                SpecialistKind::Security,
                SpecialistKind::Performance,
            ];
            if batch_has_rules {
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
    }

    #[test]
    fn role_blocks_are_tagged_and_style_exclusion_matches_the_text() {
        for kind in [Generalist, Bug, Security, Performance, Rules] {
            let role = kind.role_block();
            assert!(role.starts_with("<Role>\n"), "{kind:?}");
            assert!(role.ends_with("\n</Role>"), "{kind:?}");
            assert_eq!(
                role.contains("do not report pure style"),
                kind.excludes_pure_style(),
                "{kind:?}"
            );
        }
        assert_eq!(Performance.label(), "Performance");
    }
}
