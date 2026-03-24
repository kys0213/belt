use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Spec lifecycle status.
///
/// ```text
/// Draft -> Active -> [Paused <-> Active] -> Completing -> Completed
///                                               |
///                                               └-> Active (gap found)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecStatus {
    Draft,
    Active,
    Paused,
    /// Gap-detection found no gaps and all linked issues are Done.
    /// Awaiting test execution and HITL final confirmation.
    Completing,
    Completed,
}

impl SpecStatus {
    /// Returns the string representation of this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            SpecStatus::Draft => "draft",
            SpecStatus::Active => "active",
            SpecStatus::Paused => "paused",
            SpecStatus::Completing => "completing",
            SpecStatus::Completed => "completed",
        }
    }

    /// Returns `true` if the status is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, SpecStatus::Completed)
    }

    /// Returns `true` if transitioning from `self` to `to` is valid.
    pub fn can_transition_to(&self, to: &SpecStatus) -> bool {
        is_valid_spec_transition(*self, *to)
    }
}

impl std::str::FromStr for SpecStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(SpecStatus::Draft),
            "active" => Ok(SpecStatus::Active),
            "paused" => Ok(SpecStatus::Paused),
            "completing" => Ok(SpecStatus::Completing),
            "completed" => Ok(SpecStatus::Completed),
            _ => Err(format!("invalid spec status: {s}")),
        }
    }
}

impl fmt::Display for SpecStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validate a spec status transition.
///
/// Valid transitions:
/// - Draft -> Active
/// - Active -> Paused
/// - Active -> Completing (gap-detection finds no gaps, all linked issues Done)
/// - Paused -> Active
/// - Completing -> Completed (HITL final approval)
/// - Completing -> Active (gap found during re-check or test failure)
pub fn is_valid_spec_transition(from: SpecStatus, to: SpecStatus) -> bool {
    use SpecStatus::*;
    matches!(
        (from, to),
        (Draft, Active)
            | (Active, Paused)
            | (Active, Completing)
            | (Paused, Active)
            | (Completing, Completed)
            | (Completing, Active)
    )
}

/// Attempt a spec status transition, returning an error if invalid.
pub fn transit_spec(from: SpecStatus, to: SpecStatus) -> Result<(), SpecTransitionError> {
    if from == to {
        return Err(SpecTransitionError::SameStatus(from));
    }
    if is_valid_spec_transition(from, to) {
        Ok(())
    } else {
        Err(SpecTransitionError::Invalid { from, to })
    }
}

/// Error type for invalid spec status transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecTransitionError {
    SameStatus(SpecStatus),
    Invalid { from: SpecStatus, to: SpecStatus },
}

impl fmt::Display for SpecTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecTransitionError::SameStatus(s) => {
                write!(f, "cannot transit to same status: {s}")
            }
            SpecTransitionError::Invalid { from, to } => {
                write!(f, "invalid spec transition: {from} -> {to}")
            }
        }
    }
}

impl std::error::Error for SpecTransitionError {}

/// A spec represents a planned unit of work with lifecycle management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// Unique identifier.
    pub id: String,
    /// Associated workspace identifier.
    pub workspace_id: String,
    /// Human-readable name.
    pub name: String,
    /// Current lifecycle status.
    pub status: SpecStatus,
    /// Spec content / description.
    pub content: String,
    /// Optional priority (lower is higher priority).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Optional comma-separated labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<String>,
    /// Optional comma-separated IDs of specs this depends on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<String>,
    /// Optional comma-separated file/module paths this spec touches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
}

impl Spec {
    /// Create a new spec in Draft status.
    pub fn new(id: String, workspace_id: String, name: String, content: String) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id,
            workspace_id,
            name,
            status: SpecStatus::Draft,
            content,
            priority: None,
            labels: None,
            depends_on: None,
            entry_point: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Attempt to transition this spec to a new status.
    ///
    /// Returns the previous status on success, or an error if the transition
    /// is invalid.
    pub fn transition_to(&mut self, to: SpecStatus) -> Result<SpecStatus, SpecTransitionError> {
        transit_spec(self.status, to)?;
        let previous = self.status;
        self.status = to;
        self.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(previous)
    }

    /// Parse the comma-separated `entry_point` field into individual paths.
    pub fn entry_points(&self) -> Vec<&str> {
        match &self.entry_point {
            Some(ep) => ep.split(',').map(str::trim).filter(|s| !s.is_empty()).collect(),
            None => Vec::new(),
        }
    }
}

/// Type of overlap detected between specs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapType {
    /// Two specs share the exact same file path.
    File,
    /// One spec's path is a parent module of another's.
    Module,
}

impl fmt::Display for OverlapType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlapType::File => f.write_str("file"),
            OverlapType::Module => f.write_str("module"),
        }
    }
}

/// A detected conflict between two specs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecConflict {
    /// ID of the existing spec that conflicts.
    pub existing_spec_id: String,
    /// Name of the existing spec.
    pub existing_spec_name: String,
    /// The type of overlap.
    pub overlap_type: OverlapType,
    /// The path that caused the overlap.
    pub path: String,
}

/// Detects implicit conflicts between specs based on entry_point overlaps.
///
/// When a new spec is added, its entry points are compared against existing
/// specs to find file-level or module-level overlaps that could cause
/// merge conflicts or unintended interactions.
pub struct ConflictDetector;

impl ConflictDetector {
    /// Detect conflicts between a new spec and a list of existing specs.
    ///
    /// Returns a list of `SpecConflict` entries for each overlap found.
    /// An empty list means no conflicts were detected.
    pub fn detect(new_spec: &Spec, existing_specs: &[Spec]) -> Vec<SpecConflict> {
        let new_entry_points = new_spec.entry_points();
        if new_entry_points.is_empty() {
            return Vec::new();
        }

        // Build a map: path -> (spec_id, spec_name) for all existing, non-terminal specs
        let mut path_map: HashMap<&str, (&str, &str)> = HashMap::new();
        let mut module_entries: Vec<(&str, &str, &str)> = Vec::new(); // (path, spec_id, spec_name)

        for spec in existing_specs {
            if spec.id == new_spec.id || spec.status.is_terminal() {
                continue;
            }
            for ep in spec.entry_points() {
                path_map.insert(ep, (&spec.id, &spec.name));
                module_entries.push((ep, &spec.id, &spec.name));
            }
        }

        let mut conflicts = Vec::new();

        for new_ep in &new_entry_points {
            // Check exact file overlap
            if let Some(&(spec_id, spec_name)) = path_map.get(new_ep) {
                conflicts.push(SpecConflict {
                    existing_spec_id: spec_id.to_string(),
                    existing_spec_name: spec_name.to_string(),
                    overlap_type: OverlapType::File,
                    path: new_ep.to_string(),
                });
                continue;
            }

            // Check module overlap (parent/child directory relationship)
            let new_path = Path::new(new_ep);
            for &(existing_ep, spec_id, spec_name) in &module_entries {
                let existing_path = Path::new(existing_ep);
                if new_path.starts_with(existing_path) || existing_path.starts_with(new_path) {
                    conflicts.push(SpecConflict {
                        existing_spec_id: spec_id.to_string(),
                        existing_spec_name: spec_name.to_string(),
                        overlap_type: OverlapType::Module,
                        path: new_ep.to_string(),
                    });
                }
            }
        }

        conflicts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SpecStatus::*;

    #[test]
    fn valid_transitions() {
        assert!(transit_spec(Draft, Active).is_ok());
        assert!(transit_spec(Active, Paused).is_ok());
        assert!(transit_spec(Active, Completing).is_ok());
        assert!(transit_spec(Paused, Active).is_ok());
        assert!(transit_spec(Completing, Completed).is_ok());
        assert!(transit_spec(Completing, Active).is_ok());
    }

    #[test]
    fn invalid_transitions() {
        assert!(transit_spec(Draft, Paused).is_err());
        assert!(transit_spec(Draft, Completed).is_err());
        assert!(transit_spec(Draft, Completing).is_err());
        assert!(transit_spec(Paused, Completed).is_err());
        assert!(transit_spec(Paused, Completing).is_err());
        assert!(transit_spec(Paused, Draft).is_err());
        assert!(transit_spec(Active, Completed).is_err());
        assert!(transit_spec(Completed, Active).is_err());
        assert!(transit_spec(Completed, Draft).is_err());
        assert!(transit_spec(Completed, Completing).is_err());
        assert!(transit_spec(Completing, Draft).is_err());
        assert!(transit_spec(Completing, Paused).is_err());
    }

    #[test]
    fn same_status_rejected() {
        let statuses = [Draft, Active, Paused, Completing, Completed];
        for s in statuses {
            assert_eq!(
                transit_spec(s, s).unwrap_err(),
                SpecTransitionError::SameStatus(s)
            );
        }
    }

    #[test]
    fn exhaustive_transition_count() {
        let statuses = [Draft, Active, Paused, Completing, Completed];
        let valid_count = statuses
            .iter()
            .flat_map(|&from| statuses.iter().map(move |&to| (from, to)))
            .filter(|&(from, to)| is_valid_spec_transition(from, to))
            .count();
        assert_eq!(valid_count, 6);
    }

    #[test]
    fn status_roundtrip() {
        let statuses = [Draft, Active, Paused, Completing, Completed];
        for s in statuses {
            let str_val = s.to_string();
            let parsed: SpecStatus = str_val.parse().unwrap();
            assert_eq!(s, parsed);
        }
    }

    #[test]
    fn serde_json_roundtrip() {
        let status = SpecStatus::Active;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"active\"");
        let parsed: SpecStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn terminal_status() {
        assert!(Completed.is_terminal());
        assert!(!Draft.is_terminal());
        assert!(!Active.is_terminal());
        assert!(!Paused.is_terminal());
        assert!(!Completing.is_terminal());
    }

    #[test]
    fn new_spec_is_draft() {
        let spec = Spec::new(
            "spec-1".to_string(),
            "ws-1".to_string(),
            "My Spec".to_string(),
            "Some content".to_string(),
        );
        assert_eq!(spec.status, Draft);
    }

    #[test]
    fn spec_transition_method() {
        let mut spec = Spec::new(
            "spec-1".to_string(),
            "ws-1".to_string(),
            "My Spec".to_string(),
            "content".to_string(),
        );
        let prev = spec.transition_to(Active).unwrap();
        assert_eq!(prev, Draft);
        assert_eq!(spec.status, Active);

        let prev = spec.transition_to(Paused).unwrap();
        assert_eq!(prev, Active);
        assert_eq!(spec.status, Paused);

        let prev = spec.transition_to(Active).unwrap();
        assert_eq!(prev, Paused);

        let prev = spec.transition_to(Completing).unwrap();
        assert_eq!(prev, Active);
        assert_eq!(spec.status, Completing);

        let prev = spec.transition_to(Completed).unwrap();
        assert_eq!(prev, Completing);
        assert_eq!(spec.status, Completed);
    }

    #[test]
    fn spec_transition_invalid() {
        let mut spec = Spec::new(
            "spec-1".to_string(),
            "ws-1".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        assert!(spec.transition_to(Completed).is_err());
    }

    #[test]
    fn spec_full_json_roundtrip() {
        let mut spec = Spec::new(
            "spec-1".to_string(),
            "ws-1".to_string(),
            "My Spec".to_string(),
            "content here".to_string(),
        );
        spec.priority = Some(1);
        spec.labels = Some("bug,urgent".to_string());
        spec.depends_on = Some("spec-0".to_string());
        spec.entry_point = Some("src/auth/mod.rs,src/auth/token.rs".to_string());

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, spec.id);
        assert_eq!(parsed.priority, Some(1));
        assert_eq!(parsed.labels.as_deref(), Some("bug,urgent"));
        assert_eq!(parsed.depends_on.as_deref(), Some("spec-0"));
        assert_eq!(
            parsed.entry_point.as_deref(),
            Some("src/auth/mod.rs,src/auth/token.rs")
        );
    }

    #[test]
    fn entry_points_parses_comma_separated() {
        let mut spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        spec.entry_point = Some("src/a.rs, src/b.rs,src/c.rs".to_string());
        assert_eq!(spec.entry_points(), vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn entry_points_empty_when_none() {
        let spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        assert!(spec.entry_points().is_empty());
    }

    fn make_spec_with_entry(id: &str, name: &str, entry_point: Option<&str>) -> Spec {
        let mut spec = Spec::new(
            id.to_string(),
            "ws-1".to_string(),
            name.to_string(),
            "content".to_string(),
        );
        spec.entry_point = entry_point.map(String::from);
        spec.status = Active;
        spec
    }

    #[test]
    fn conflict_detector_no_conflicts() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/new.rs"));
        let existing = vec![make_spec_with_entry("s1", "old", Some("src/old.rs"))];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn conflict_detector_file_overlap() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/auth.rs"));
        let existing = vec![make_spec_with_entry("s1", "old", Some("src/auth.rs"))];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].overlap_type, OverlapType::File);
        assert_eq!(conflicts[0].existing_spec_id, "s1");
        assert_eq!(conflicts[0].path, "src/auth.rs");
    }

    #[test]
    fn conflict_detector_module_overlap() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/auth/token.rs"));
        let existing = vec![make_spec_with_entry("s1", "old", Some("src/auth"))];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].overlap_type, OverlapType::Module);
    }

    #[test]
    fn conflict_detector_reverse_module_overlap() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/auth"));
        let existing = vec![make_spec_with_entry("s1", "old", Some("src/auth/token.rs"))];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].overlap_type, OverlapType::Module);
    }

    #[test]
    fn conflict_detector_skips_completed_specs() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/auth.rs"));
        let mut completed = make_spec_with_entry("s1", "old", Some("src/auth.rs"));
        completed.status = Completed;
        let conflicts = ConflictDetector::detect(&new_spec, &[completed]);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn conflict_detector_skips_self() {
        let spec = make_spec_with_entry("s1", "self", Some("src/auth.rs"));
        let conflicts = ConflictDetector::detect(&spec, &[spec.clone()]);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn conflict_detector_no_entry_point_no_conflict() {
        let new_spec = make_spec_with_entry("s2", "new", None);
        let existing = vec![make_spec_with_entry("s1", "old", Some("src/auth.rs"))];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn conflict_detector_multiple_entry_points() {
        let new_spec = make_spec_with_entry("s2", "new", Some("src/auth.rs,src/db.rs"));
        let existing = vec![
            make_spec_with_entry("s1", "old-auth", Some("src/auth.rs")),
            make_spec_with_entry("s3", "old-api", Some("src/api.rs")),
        ];
        let conflicts = ConflictDetector::detect(&new_spec, &existing);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].existing_spec_id, "s1");
        assert_eq!(conflicts[0].path, "src/auth.rs");
    }

    #[test]
    fn overlap_type_display() {
        assert_eq!(OverlapType::File.to_string(), "file");
        assert_eq!(OverlapType::Module.to_string(), "module");
    }

    #[test]
    fn spec_conflict_json_roundtrip() {
        let conflict = SpecConflict {
            existing_spec_id: "s1".to_string(),
            existing_spec_name: "Auth".to_string(),
            overlap_type: OverlapType::File,
            path: "src/auth.rs".to_string(),
        };
        let json = serde_json::to_string(&conflict).unwrap();
        let parsed: SpecConflict = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, conflict);
    }
}
