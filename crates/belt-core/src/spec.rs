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
///
/// Any non-terminal state -> Archived (soft delete)
/// Archived -> Active (restore)
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
    /// Soft-deleted spec. Can be restored to Active via `spec resume`.
    Archived,
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
            SpecStatus::Archived => "archived",
        }
    }

    /// Returns `true` if the status is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, SpecStatus::Completed)
    }

    /// Returns `true` if this spec is archived (soft-deleted).
    pub fn is_archived(&self) -> bool {
        matches!(self, SpecStatus::Archived)
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
            "archived" => Ok(SpecStatus::Archived),
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
/// - Draft | Active | Paused | Completing -> Archived (soft delete)
/// - Archived -> Active (restore)
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
            | (Draft, Archived)
            | (Active, Archived)
            | (Paused, Archived)
            | (Completing, Archived)
            | (Archived, Active)
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

/// A link between a spec and an external resource (URL or issue ID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecLink {
    /// Unique identifier for this link.
    pub id: String,
    /// The spec this link belongs to.
    pub spec_id: String,
    /// The target resource (URL or issue reference like `owner/repo#123`).
    pub target: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

impl SpecLink {
    /// Create a new spec link.
    pub fn new(id: String, spec_id: String, target: String) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id,
            spec_id,
            target,
            created_at: now,
        }
    }
}

/// Result of verifying a single spec link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkVerification {
    /// The link that was verified.
    pub link: SpecLink,
    /// Whether the link target is reachable / valid.
    pub valid: bool,
    /// Human-readable detail (e.g. HTTP status or error message).
    pub detail: String,
}

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
    /// Optional comma-separated GitHub issue numbers created by decomposition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decomposed_issues: Option<String>,
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
            decomposed_issues: None,
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
            Some(ep) => ep
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Parse the comma-separated `decomposed_issues` field into individual issue numbers.
    pub fn decomposed_issue_numbers(&self) -> Vec<&str> {
        match &self.decomposed_issues {
            Some(issues) => issues
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns `true` if this spec was decomposed into child issues.
    pub fn is_decomposed(&self) -> bool {
        self.decomposed_issues.is_some()
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

/// Extract acceptance criteria from markdown content.
///
/// Looks for a section headed by `## Acceptance Criteria` or `## AC`
/// and collects the list items (lines starting with `- ` or `* `) that follow
/// until the next heading or end of content.
///
/// Returns a list of acceptance criteria strings (without the leading bullet).
pub fn extract_acceptance_criteria(content: &str) -> Vec<String> {
    let mut in_ac_section = false;
    let mut criteria = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect AC section header
        if trimmed.starts_with("## ") {
            let header = trimmed.trim_start_matches("## ").trim();
            if header.eq_ignore_ascii_case("acceptance criteria")
                || header.eq_ignore_ascii_case("ac")
            {
                in_ac_section = true;
                continue;
            } else if in_ac_section {
                // Another heading encountered, stop collecting
                break;
            }
        }

        // Also stop on higher-level headings
        if in_ac_section && trimmed.starts_with("# ") && !trimmed.starts_with("## ") {
            break;
        }

        if in_ac_section {
            // Collect bullet items (- or *)
            if let Some(rest) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                let text = rest.trim().to_string();
                if !text.is_empty() {
                    criteria.push(text);
                }
            }
        }
    }

    criteria
}

/// Check whether all decomposed issues are closed, indicating the spec
/// is ready to transition to `Completing`.
///
/// `issue_states` should contain `(issue_number, is_closed)` pairs for each
/// decomposed issue. Returns `true` when all issues are closed and the list
/// is non-empty.
pub fn all_decomposed_issues_closed(issue_states: &[(String, bool)]) -> bool {
    !issue_states.is_empty() && issue_states.iter().all(|(_, closed)| *closed)
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
        // Archived transitions
        assert!(transit_spec(Draft, Archived).is_ok());
        assert!(transit_spec(Active, Archived).is_ok());
        assert!(transit_spec(Paused, Archived).is_ok());
        assert!(transit_spec(Completing, Archived).is_ok());
        assert!(transit_spec(Archived, Active).is_ok());
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
        // Archived invalid transitions
        assert!(transit_spec(Completed, Archived).is_err());
        assert!(transit_spec(Archived, Draft).is_err());
        assert!(transit_spec(Archived, Paused).is_err());
        assert!(transit_spec(Archived, Completing).is_err());
        assert!(transit_spec(Archived, Completed).is_err());
    }

    #[test]
    fn same_status_rejected() {
        let statuses = [Draft, Active, Paused, Completing, Completed, Archived];
        for s in statuses {
            assert_eq!(
                transit_spec(s, s).unwrap_err(),
                SpecTransitionError::SameStatus(s)
            );
        }
    }

    #[test]
    fn exhaustive_transition_count() {
        let statuses = [Draft, Active, Paused, Completing, Completed, Archived];
        let valid_count = statuses
            .iter()
            .flat_map(|&from| statuses.iter().map(move |&to| (from, to)))
            .filter(|&(from, to)| is_valid_spec_transition(from, to))
            .count();
        // 6 original + 5 archived (4 -> Archived, 1 Archived -> Active)
        assert_eq!(valid_count, 11);
    }

    #[test]
    fn status_roundtrip() {
        let statuses = [Draft, Active, Paused, Completing, Completed, Archived];
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
        assert!(!Archived.is_terminal());
    }

    #[test]
    fn archived_status() {
        assert!(Archived.is_archived());
        assert!(!Draft.is_archived());
        assert!(!Active.is_archived());
        assert!(!Completed.is_archived());
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
    fn spec_link_new() {
        let link = SpecLink::new(
            "link-1".to_string(),
            "spec-1".to_string(),
            "https://github.com/org/repo/issues/42".to_string(),
        );
        assert_eq!(link.id, "link-1");
        assert_eq!(link.spec_id, "spec-1");
        assert_eq!(link.target, "https://github.com/org/repo/issues/42");
        assert!(!link.created_at.is_empty());
    }

    #[test]
    fn spec_link_json_roundtrip() {
        let link = SpecLink::new(
            "link-1".to_string(),
            "spec-1".to_string(),
            "https://example.com".to_string(),
        );
        let json = serde_json::to_string(&link).unwrap();
        let parsed: SpecLink = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, link.id);
        assert_eq!(parsed.spec_id, link.spec_id);
        assert_eq!(parsed.target, link.target);
    }

    #[test]
    fn link_verification_json_roundtrip() {
        let link = SpecLink::new(
            "link-1".to_string(),
            "spec-1".to_string(),
            "https://example.com".to_string(),
        );
        let verification = LinkVerification {
            link,
            valid: true,
            detail: "200 OK".to_string(),
        };
        let json = serde_json::to_string(&verification).unwrap();
        let parsed: LinkVerification = serde_json::from_str(&json).unwrap();
        assert!(parsed.valid);
        assert_eq!(parsed.detail, "200 OK");
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
        assert_eq!(
            spec.entry_points(),
            vec!["src/a.rs", "src/b.rs", "src/c.rs"]
        );
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

    #[test]
    fn extract_ac_basic() {
        let content = "# Overview\nSome description.\n\n## Acceptance Criteria\n- Login endpoint works\n- Token refresh works\n- Logout endpoint works\n\n## Notes\nExtra info.";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(
            ac,
            vec![
                "Login endpoint works",
                "Token refresh works",
                "Logout endpoint works",
            ]
        );
    }

    #[test]
    fn extract_ac_short_header() {
        let content = "## AC\n* First criterion\n* Second criterion\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac, vec!["First criterion", "Second criterion"]);
    }

    #[test]
    fn extract_ac_no_section() {
        let content = "# Overview\nSome description.\n## Design\n- Item 1\n";
        let ac = extract_acceptance_criteria(content);
        assert!(ac.is_empty());
    }

    #[test]
    fn extract_ac_empty_bullets_skipped() {
        let content = "## Acceptance Criteria\n-  \n- Valid item\n- \n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac, vec!["Valid item"]);
    }

    #[test]
    fn extract_ac_case_insensitive_header() {
        let content = "## acceptance criteria\n- Item A\n- Item B\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac, vec!["Item A", "Item B"]);
    }

    #[test]
    fn extract_ac_stops_at_next_heading() {
        let content = "## AC\n- AC item\n## Other Section\n- Not AC\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac, vec!["AC item"]);
    }

    #[test]
    fn extract_ac_mixed_bullets() {
        let content = "## Acceptance Criteria\n- Dash item\n* Star item\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac, vec!["Dash item", "Star item"]);
    }

    #[test]
    fn decomposed_issues_none_by_default() {
        let spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        assert!(spec.decomposed_issues.is_none());
        assert!(!spec.is_decomposed());
        assert!(spec.decomposed_issue_numbers().is_empty());
    }

    #[test]
    fn decomposed_issues_parses_comma_separated() {
        let mut spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        spec.decomposed_issues = Some("42,43,44".to_string());
        assert!(spec.is_decomposed());
        assert_eq!(spec.decomposed_issue_numbers(), vec!["42", "43", "44"]);
    }

    #[test]
    fn decomposed_issues_handles_whitespace() {
        let mut spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        spec.decomposed_issues = Some("42, 43, 44".to_string());
        assert_eq!(spec.decomposed_issue_numbers(), vec!["42", "43", "44"]);
    }

    #[test]
    fn all_decomposed_closed_returns_true_when_all_closed() {
        let states = vec![("42".to_string(), true), ("43".to_string(), true)];
        assert!(all_decomposed_issues_closed(&states));
    }

    #[test]
    fn all_decomposed_closed_returns_false_when_some_open() {
        let states = vec![("42".to_string(), true), ("43".to_string(), false)];
        assert!(!all_decomposed_issues_closed(&states));
    }

    #[test]
    fn all_decomposed_closed_returns_false_when_empty() {
        let states: Vec<(String, bool)> = vec![];
        assert!(!all_decomposed_issues_closed(&states));
    }

    #[test]
    fn decomposed_spec_json_roundtrip() {
        let mut spec = Spec::new(
            "s1".to_string(),
            "ws".to_string(),
            "name".to_string(),
            "content".to_string(),
        );
        spec.decomposed_issues = Some("10,20,30".to_string());
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.decomposed_issues, Some("10,20,30".to_string()));
    }
}
