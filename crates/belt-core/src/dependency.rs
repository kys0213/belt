use crate::spec::{Spec, SpecStatus};

/// Result of a dependency check for a single spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyCheckResult {
    /// All dependencies are satisfied (completed).
    Satisfied,
    /// One or more dependencies are not yet completed.
    Blocked {
        /// IDs of the specs that are still pending/incomplete.
        pending_deps: Vec<String>,
    },
    /// No dependencies declared — always satisfies.
    NoDependencies,
}

impl DependencyCheckResult {
    /// Returns `true` if execution is allowed (satisfied or no dependencies).
    pub fn is_ready(&self) -> bool {
        matches!(
            self,
            DependencyCheckResult::Satisfied | DependencyCheckResult::NoDependencies
        )
    }
}

/// Trait for checking whether a spec's dependencies have been met.
///
/// The daemon calls `check_dependencies` before promoting an item
/// from Pending to Ready. If any dependency spec is not yet Completed,
/// the item stays in Pending.
pub trait DependencyGuard {
    /// Check whether all specs listed in `spec.depends_on` are completed.
    ///
    /// `resolve_spec` is a callback that looks up a spec by its ID.
    /// This avoids coupling the guard to any particular storage backend.
    fn check_dependencies<F>(&self, spec: &Spec, resolve_spec: F) -> DependencyCheckResult
    where
        F: Fn(&str) -> Option<Spec>;
}

/// Default implementation that blocks execution when any dependency
/// spec has not reached [`SpecStatus::Completed`].
#[derive(Debug, Default)]
pub struct SpecDependencyGuard;

impl DependencyGuard for SpecDependencyGuard {
    fn check_dependencies<F>(&self, spec: &Spec, resolve_spec: F) -> DependencyCheckResult
    where
        F: Fn(&str) -> Option<Spec>,
    {
        let deps_str = match &spec.depends_on {
            Some(s) if !s.trim().is_empty() => s,
            _ => return DependencyCheckResult::NoDependencies,
        };

        let dep_ids: Vec<&str> = deps_str.split(',').map(|s| s.trim()).collect();

        let pending_deps: Vec<String> = dep_ids
            .into_iter()
            .filter(|id| {
                match resolve_spec(id) {
                    Some(dep_spec) => dep_spec.status != SpecStatus::Completed,
                    // If the dependency spec is not found, treat it as blocking
                    // (safe default per HITL principle).
                    None => true,
                }
            })
            .map(|id| id.to_string())
            .collect();

        if pending_deps.is_empty() {
            DependencyCheckResult::Satisfied
        } else {
            DependencyCheckResult::Blocked { pending_deps }
        }
    }
}

/// Parse a `depends_on` string into individual spec IDs.
pub fn parse_depends_on(depends_on: &str) -> Vec<&str> {
    depends_on
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Spec;

    fn make_spec(id: &str, status: SpecStatus, depends_on: Option<&str>) -> Spec {
        let mut spec = Spec::new(
            id.to_string(),
            "ws-1".to_string(),
            format!("Spec {id}"),
            "content".to_string(),
        );
        spec.status = status;
        spec.depends_on = depends_on.map(|s| s.to_string());
        spec
    }

    #[test]
    fn no_dependencies_is_ready() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s1", SpecStatus::Active, None);
        let result = guard.check_dependencies(&spec, |_| None);
        assert_eq!(result, DependencyCheckResult::NoDependencies);
        assert!(result.is_ready());
    }

    #[test]
    fn empty_depends_on_is_ready() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s1", SpecStatus::Active, Some(""));
        let result = guard.check_dependencies(&spec, |_| None);
        assert_eq!(result, DependencyCheckResult::NoDependencies);
        assert!(result.is_ready());
    }

    #[test]
    fn whitespace_only_depends_on_is_ready() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s1", SpecStatus::Active, Some("  "));
        let result = guard.check_dependencies(&spec, |_| None);
        assert_eq!(result, DependencyCheckResult::NoDependencies);
    }

    #[test]
    fn all_deps_completed_is_satisfied() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s3", SpecStatus::Active, Some("s1,s2"));
        let dep1 = make_spec("s1", SpecStatus::Completed, None);
        let dep2 = make_spec("s2", SpecStatus::Completed, None);

        let result = guard.check_dependencies(&spec, |id| match id {
            "s1" => Some(dep1.clone()),
            "s2" => Some(dep2.clone()),
            _ => None,
        });
        assert_eq!(result, DependencyCheckResult::Satisfied);
        assert!(result.is_ready());
    }

    #[test]
    fn incomplete_dep_blocks() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s3", SpecStatus::Active, Some("s1,s2"));
        let dep1 = make_spec("s1", SpecStatus::Completed, None);
        let dep2 = make_spec("s2", SpecStatus::Active, None);

        let result = guard.check_dependencies(&spec, |id| match id {
            "s1" => Some(dep1.clone()),
            "s2" => Some(dep2.clone()),
            _ => None,
        });
        assert_eq!(
            result,
            DependencyCheckResult::Blocked {
                pending_deps: vec!["s2".to_string()]
            }
        );
        assert!(!result.is_ready());
    }

    #[test]
    fn missing_dep_blocks_safe_default() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s2", SpecStatus::Active, Some("s1"));

        let result = guard.check_dependencies(&spec, |_| None);
        assert_eq!(
            result,
            DependencyCheckResult::Blocked {
                pending_deps: vec!["s1".to_string()]
            }
        );
        assert!(!result.is_ready());
    }

    #[test]
    fn depends_on_with_whitespace_trimmed() {
        let guard = SpecDependencyGuard;
        let spec = make_spec("s2", SpecStatus::Active, Some(" s1 , s0 "));
        let dep1 = make_spec("s1", SpecStatus::Completed, None);
        let dep0 = make_spec("s0", SpecStatus::Completed, None);

        let result = guard.check_dependencies(&spec, |id| match id {
            "s1" => Some(dep1.clone()),
            "s0" => Some(dep0.clone()),
            _ => None,
        });
        assert_eq!(result, DependencyCheckResult::Satisfied);
    }

    #[test]
    fn parse_depends_on_basic() {
        assert_eq!(parse_depends_on("s1,s2,s3"), vec!["s1", "s2", "s3"]);
    }

    #[test]
    fn parse_depends_on_with_whitespace() {
        assert_eq!(parse_depends_on(" s1 , s2 "), vec!["s1", "s2"]);
    }

    #[test]
    fn parse_depends_on_empty_entries_filtered() {
        assert_eq!(parse_depends_on("s1,,s2,"), vec!["s1", "s2"]);
    }
}
