use crate::spec::{OverlapType, Spec, SpecConflict, SpecStatus};

/// Result of a conflict check between specs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictCheckResult {
    /// No conflicts detected.
    Clear,
    /// Conflicting specs share overlapping entry_point paths.
    Conflict {
        /// IDs of the specs that overlap with the checked spec.
        conflicting_specs: Vec<String>,
        /// The overlapping entry_point paths.
        overlapping_paths: Vec<String>,
    },
}

impl ConflictCheckResult {
    /// Returns `true` if no conflict was detected.
    pub fn is_clear(&self) -> bool {
        matches!(self, ConflictCheckResult::Clear)
    }
}

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

    /// Check whether a spec's `entry_point` paths conflict with other active specs.
    ///
    /// Two specs conflict when their `entry_point` fields share one or more
    /// overlapping file/module paths. The `other_specs` callback returns all
    /// active/running specs that should be checked against.
    fn check_conflicts<F>(&self, spec: &Spec, other_specs: F) -> ConflictCheckResult
    where
        F: Fn() -> Vec<Spec>;
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

    fn check_conflicts<F>(&self, spec: &Spec, other_specs: F) -> ConflictCheckResult
    where
        F: Fn() -> Vec<Spec>,
    {
        let my_paths = parse_entry_points(spec);
        if my_paths.is_empty() {
            return ConflictCheckResult::Clear;
        }

        let others = other_specs();
        let mut conflicting_specs: Vec<String> = Vec::new();
        let mut overlapping_paths: Vec<String> = Vec::new();

        for other in &others {
            if other.id == spec.id {
                continue;
            }
            let other_paths = parse_entry_points(other);
            for path in &my_paths {
                if other_paths.contains(path) && !overlapping_paths.contains(path) {
                    overlapping_paths.push(path.clone());
                    if !conflicting_specs.contains(&other.id) {
                        conflicting_specs.push(other.id.clone());
                    }
                }
            }
        }

        if conflicting_specs.is_empty() {
            ConflictCheckResult::Clear
        } else {
            ConflictCheckResult::Conflict {
                conflicting_specs,
                overlapping_paths,
            }
        }
    }
}

/// Parse a spec's `entry_point` field into individual trimmed paths.
fn parse_entry_points(spec: &Spec) -> Vec<String> {
    match &spec.entry_point {
        Some(ep) if !ep.trim().is_empty() => ep
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
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

/// Action to take when a spec conflict is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// Automatically register the existing spec as a dependency.
    /// Used for module-level overlaps where ordering is sufficient.
    AutoDependency {
        /// ID of the existing spec to depend on.
        dependency_spec_id: String,
    },
    /// Escalate to human review via HITL.
    /// Used for file-level overlaps where automatic resolution is unsafe.
    Hitl {
        /// Description of why HITL is needed.
        reason: String,
    },
}

/// Resolved conflict pairing a detected conflict with its recommended action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolution {
    /// The original detected conflict.
    pub conflict: SpecConflict,
    /// The recommended action.
    pub action: ConflictAction,
}

/// Resolve a list of spec conflicts into actionable resolutions.
///
/// Resolution strategy:
/// - **File overlap** (`OverlapType::File`): Escalate to HITL. Two specs
///   modifying the exact same file are likely to produce merge conflicts
///   that require human judgment.
/// - **Module overlap** (`OverlapType::Module`): Auto-register the existing
///   spec as a dependency. Parent/child directory relationships can be
///   safely serialized by ensuring the existing spec completes first.
pub fn resolve_conflicts(conflicts: &[SpecConflict]) -> Vec<ConflictResolution> {
    conflicts
        .iter()
        .map(|conflict| {
            let action = match conflict.overlap_type {
                OverlapType::File => ConflictAction::Hitl {
                    reason: format!(
                        "file-level overlap at '{}' with spec '{}' ({}): requires human review",
                        conflict.path, conflict.existing_spec_name, conflict.existing_spec_id,
                    ),
                },
                OverlapType::Module => ConflictAction::AutoDependency {
                    dependency_spec_id: conflict.existing_spec_id.clone(),
                },
            };
            ConflictResolution {
                conflict: conflict.clone(),
                action,
            }
        })
        .collect()
}

/// Append dependency spec IDs to an existing `depends_on` string.
///
/// Deduplicates IDs and returns the updated comma-separated string.
pub fn append_dependencies(current: Option<&str>, new_dep_ids: &[&str]) -> Option<String> {
    let mut existing: Vec<String> = current
        .map(|s| parse_depends_on(s).into_iter().map(String::from).collect())
        .unwrap_or_default();

    for id in new_dep_ids {
        if !existing.iter().any(|e| e == id) {
            existing.push(id.to_string());
        }
    }

    if existing.is_empty() {
        None
    } else {
        Some(existing.join(","))
    }
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

    fn make_spec_with_entry(id: &str, status: SpecStatus, entry_point: Option<&str>) -> Spec {
        let mut spec = Spec::new(
            id.to_string(),
            "ws-1".to_string(),
            format!("Spec {id}"),
            "content".to_string(),
        );
        spec.status = status;
        spec.entry_point = entry_point.map(|s| s.to_string());
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

    // ---- check_conflicts tests ----

    #[test]
    fn no_entry_point_is_clear() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry("s1", SpecStatus::Active, None);
        let result = guard.check_conflicts(&spec, || vec![]);
        assert_eq!(result, ConflictCheckResult::Clear);
        assert!(result.is_clear());
    }

    #[test]
    fn empty_entry_point_is_clear() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry("s1", SpecStatus::Active, Some(""));
        let result = guard.check_conflicts(&spec, || vec![]);
        assert_eq!(result, ConflictCheckResult::Clear);
    }

    #[test]
    fn no_overlap_is_clear() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry("s1", SpecStatus::Active, Some("src/auth/mod.rs"));
        let other = make_spec_with_entry("s2", SpecStatus::Active, Some("src/db/mod.rs"));
        let result = guard.check_conflicts(&spec, || vec![other.clone()]);
        assert_eq!(result, ConflictCheckResult::Clear);
    }

    #[test]
    fn overlapping_entry_point_detects_conflict() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry(
            "s1",
            SpecStatus::Active,
            Some("src/auth/mod.rs,src/db/mod.rs"),
        );
        let other = make_spec_with_entry("s2", SpecStatus::Active, Some("src/auth/mod.rs"));
        let result = guard.check_conflicts(&spec, || vec![other.clone()]);
        assert_eq!(
            result,
            ConflictCheckResult::Conflict {
                conflicting_specs: vec!["s2".to_string()],
                overlapping_paths: vec!["src/auth/mod.rs".to_string()],
            }
        );
        assert!(!result.is_clear());
    }

    #[test]
    fn self_is_excluded_from_conflict_check() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry("s1", SpecStatus::Active, Some("src/auth/mod.rs"));
        let same = make_spec_with_entry("s1", SpecStatus::Active, Some("src/auth/mod.rs"));
        let result = guard.check_conflicts(&spec, || vec![same.clone()]);
        assert_eq!(result, ConflictCheckResult::Clear);
    }

    #[test]
    fn multiple_conflicts_detected() {
        let guard = SpecDependencyGuard;
        let spec = make_spec_with_entry(
            "s1",
            SpecStatus::Active,
            Some("src/auth/mod.rs,src/db/mod.rs"),
        );
        let other1 = make_spec_with_entry("s2", SpecStatus::Active, Some("src/auth/mod.rs"));
        let other2 = make_spec_with_entry("s3", SpecStatus::Active, Some("src/db/mod.rs"));
        let result = guard.check_conflicts(&spec, || vec![other1.clone(), other2.clone()]);
        match result {
            ConflictCheckResult::Conflict {
                conflicting_specs,
                overlapping_paths,
            } => {
                assert_eq!(conflicting_specs.len(), 2);
                assert!(conflicting_specs.contains(&"s2".to_string()));
                assert!(conflicting_specs.contains(&"s3".to_string()));
                assert_eq!(overlapping_paths.len(), 2);
            }
            ConflictCheckResult::Clear => panic!("expected conflict"),
        }
    }

    // ---- resolve_conflicts tests ----

    #[test]
    fn resolve_file_conflict_produces_hitl() {
        let conflict = SpecConflict {
            existing_spec_id: "s1".to_string(),
            existing_spec_name: "Auth".to_string(),
            overlap_type: OverlapType::File,
            path: "src/auth.rs".to_string(),
        };
        let resolutions = resolve_conflicts(&[conflict.clone()]);
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].conflict, conflict);
        assert!(matches!(resolutions[0].action, ConflictAction::Hitl { .. }));
    }

    #[test]
    fn resolve_module_conflict_produces_auto_dependency() {
        let conflict = SpecConflict {
            existing_spec_id: "s1".to_string(),
            existing_spec_name: "Auth".to_string(),
            overlap_type: OverlapType::Module,
            path: "src/auth/token.rs".to_string(),
        };
        let resolutions = resolve_conflicts(&[conflict.clone()]);
        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0].action,
            ConflictAction::AutoDependency {
                dependency_spec_id: "s1".to_string()
            }
        );
    }

    #[test]
    fn resolve_mixed_conflicts() {
        let file_conflict = SpecConflict {
            existing_spec_id: "s1".to_string(),
            existing_spec_name: "Auth".to_string(),
            overlap_type: OverlapType::File,
            path: "src/auth.rs".to_string(),
        };
        let module_conflict = SpecConflict {
            existing_spec_id: "s2".to_string(),
            existing_spec_name: "DB".to_string(),
            overlap_type: OverlapType::Module,
            path: "src/db/mod.rs".to_string(),
        };
        let resolutions = resolve_conflicts(&[file_conflict, module_conflict]);
        assert_eq!(resolutions.len(), 2);
        assert!(matches!(resolutions[0].action, ConflictAction::Hitl { .. }));
        assert!(matches!(
            resolutions[1].action,
            ConflictAction::AutoDependency { .. }
        ));
    }

    #[test]
    fn resolve_empty_conflicts() {
        let resolutions = resolve_conflicts(&[]);
        assert!(resolutions.is_empty());
    }

    #[test]
    fn append_dependencies_to_none() {
        let result = append_dependencies(None, &["s1", "s2"]);
        assert_eq!(result.as_deref(), Some("s1,s2"));
    }

    #[test]
    fn append_dependencies_to_existing() {
        let result = append_dependencies(Some("s1"), &["s2", "s3"]);
        assert_eq!(result.as_deref(), Some("s1,s2,s3"));
    }

    #[test]
    fn append_dependencies_deduplicates() {
        let result = append_dependencies(Some("s1,s2"), &["s2", "s3"]);
        assert_eq!(result.as_deref(), Some("s1,s2,s3"));
    }

    #[test]
    fn append_dependencies_empty_new() {
        let result = append_dependencies(Some("s1"), &[]);
        assert_eq!(result.as_deref(), Some("s1"));
    }

    #[test]
    fn append_dependencies_both_empty() {
        let result = append_dependencies(None, &[]);
        assert_eq!(result, None);
    }
}
