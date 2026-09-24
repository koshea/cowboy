//! Host prerequisite checks for the sandbox, behind `cowboy doctor`.
//!
//! The container had one prerequisite — a working Docker — and the daemon reported
//! its own health. A host-native sandbox instead depends on several kernel features
//! that a distribution can each independently omit, so "why did this fail" needs an
//! answer that is more specific than "it did not start".
//!
//! Two principles here:
//!
//! - **Check by doing.** Wherever it is cheap, the check performs the real
//!   operation — spawn a user namespace, ask the kernel its Landlock ABI, load the
//!   real ruleset in a throwaway namespace. A check that reads a config symbol and
//!   infers the rest is the kind that passes on a machine where the feature does not
//!   work.
//! - **Say what to do.** A missing feature reports the remedy (a kernel option to
//!   set, a package to install), because the person reading this is being asked to
//!   change their machine.
//!
//! Nothing here is part of the security boundary. Every one of these features is
//! also checked at the point of use, where failing closed is what matters; this is
//! for diagnosis.

/// How a prerequisite came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Present and working.
    Ok,
    /// Works, but degraded — a capability is unavailable without blocking use.
    Warn,
    /// The sandbox cannot run without this.
    Missing,
}

/// One prerequisite and what we found.
#[derive(Debug, Clone)]
pub struct Requirement {
    pub name: &'static str,
    pub state: State,
    /// What was found, phrased for someone who did not write this code.
    pub detail: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
}

impl Requirement {
    pub(crate) fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            state: State::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }
    pub(crate) fn warn(
        name: &'static str,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name,
            state: State::Warn,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
    pub(crate) fn missing(
        name: &'static str,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name,
            state: State::Missing,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Whether the caller has declared that the sandbox **must** work here.
///
/// `COWBOY_SANDBOX_TESTS=required` is the repo-wide switch: the sandbox integration
/// tests use it to turn a self-skip into a failure, so a suite cannot pass by quietly
/// doing nothing precisely where it matters. The host-capability unit tests honour the
/// same variable, so there is one thing to set rather than one per test file.
pub fn tests_required() -> bool {
    std::env::var("COWBOY_SANDBOX_TESTS").as_deref() == Ok("required")
}

/// Run every check for this OS, most fundamental first: a reader should not have to
/// work out which of six failures is the cause of the others.
pub fn check_all() -> Vec<Requirement> {
    super::backend::preflight::check_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite runs on the target host, so these must all pass here. If one does
    /// not, the sandbox tests would be skipping and the failure belongs in `doctor`
    /// rather than being discovered later.
    ///
    /// Except where the sandbox genuinely cannot run — a CI runner without bubblewrap,
    /// or Ubuntu 24.04's AppArmor gate on unprivileged user namespaces. There these
    /// findings are correct, and failing on them tests the host rather than the code.
    /// `COWBOY_SANDBOX_TESTS=required` makes the skip a failure, matching the
    /// integration tests, so CI that means to cover the sandbox says so once.
    #[test]
    fn the_host_meets_every_requirement() {
        let checks = check_all();
        assert!(!checks.is_empty());
        let broken: Vec<_> = checks
            .iter()
            .filter(|c| c.state == State::Missing)
            .map(|c| {
                format!(
                    "{}: {} — {}",
                    c.name,
                    c.detail,
                    c.remedy.as_deref().unwrap_or("")
                )
            })
            .collect();
        if !broken.is_empty() && !super::tests_required() {
            eprintln!("skipping: this host cannot run the sandbox: {broken:#?}");
            return;
        }
        assert!(
            broken.is_empty(),
            "this host cannot run the sandbox: {broken:#?}"
        );
    }

    /// Every non-Ok result must say what to do. A check that reports a problem and
    /// leaves the reader to guess is not much better than the failure it replaced.
    #[test]
    fn anything_not_ok_carries_a_remedy() {
        for c in check_all() {
            match c.state {
                State::Ok => assert!(
                    c.remedy.is_none(),
                    "{}: an ok check needs no remedy",
                    c.name
                ),
                _ => assert!(
                    c.remedy.as_deref().is_some_and(|r| !r.trim().is_empty()),
                    "{} reported a problem with no remedy",
                    c.name
                ),
            }
        }
    }
}
