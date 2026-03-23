use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Escalation action 종류.
///
/// yaml에서 failure_count → action 매핑으로 정의된다.
/// retry만 on_fail을 실행하지 않는다 (silent retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationAction {
    /// 조용한 재시도 (on_fail 미실행)
    Retry,
    /// on_fail 실행 + 재시도
    RetryWithComment,
    /// on_fail 실행 + HITL 이벤트 생성
    Hitl,
    /// on_fail 실행 + Skipped 전이
    Skip,
    /// on_fail 실행 + HITL(replan) 이벤트 생성
    Replan,
}

impl EscalationAction {
    /// on_fail script를 실행해야 하는지.
    pub fn should_run_on_fail(&self) -> bool {
        !matches!(self, EscalationAction::Retry)
    }

    /// 재시도를 수행하는지 (새 아이템 생성).
    pub fn is_retry(&self) -> bool {
        matches!(
            self,
            EscalationAction::Retry | EscalationAction::RetryWithComment
        )
    }
}

/// yaml 기반 escalation 정책.
///
/// failure_count → EscalationAction 매핑.
/// `terminal` 키는 HITL timeout 시 적용되는 별도 액션.
#[derive(Debug, Clone, Default)]
pub struct EscalationPolicy {
    rules: BTreeMap<u32, EscalationAction>,
    terminal: Option<EscalationAction>,
}

impl Serialize for EscalationPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let len = self.rules.len() + usize::from(self.terminal.is_some());
        let mut map = serializer.serialize_map(Some(len))?;
        for (k, v) in &self.rules {
            map.serialize_entry(k, v)?;
        }
        if let Some(ref action) = self.terminal {
            map.serialize_entry("terminal", action)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for EscalationPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: BTreeMap<String, EscalationAction> = BTreeMap::deserialize(deserializer)?;
        let mut rules = BTreeMap::new();
        let mut terminal = None;
        for (k, v) in raw {
            if k == "terminal" {
                terminal = Some(v);
            } else {
                let key: u32 = k.parse().map_err(|_| {
                    serde::de::Error::custom(format!("invalid escalation key: {k}"))
                })?;
                rules.insert(key, v);
            }
        }
        Ok(Self { rules, terminal })
    }
}

impl EscalationPolicy {
    /// 숫자 키 규칙만으로 생성한다.
    pub fn new(rules: BTreeMap<u32, EscalationAction>) -> Self {
        Self {
            rules,
            terminal: None,
        }
    }

    /// 숫자 키 규칙과 terminal 액션을 함께 지정하여 생성한다.
    pub fn with_terminal(
        rules: BTreeMap<u32, EscalationAction>,
        terminal: EscalationAction,
    ) -> Self {
        Self {
            rules,
            terminal: Some(terminal),
        }
    }

    /// failure_count에 대응하는 escalation action을 결정한다.
    pub fn resolve(&self, failure_count: u32) -> EscalationAction {
        if self.rules.is_empty() {
            return EscalationAction::Retry;
        }

        if let Some(&action) = self.rules.get(&failure_count) {
            return action;
        }

        self.rules
            .range(..=failure_count)
            .next_back()
            .map(|(_, &action)| action)
            .unwrap_or(EscalationAction::Retry)
    }

    /// HITL timeout 시 적용되는 terminal action을 반환한다.
    pub fn terminal_action(&self) -> Option<&EscalationAction> {
        self.terminal.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// 기본 escalation 정책 (spec 기준).
///
/// ```yaml
/// 1: retry
/// 2: retry_with_comment
/// 3: hitl
/// terminal: skip
/// ```
pub fn default_escalation_policy() -> EscalationPolicy {
    let mut rules = BTreeMap::new();
    rules.insert(1, EscalationAction::Retry);
    rules.insert(2, EscalationAction::RetryWithComment);
    rules.insert(3, EscalationAction::Hitl);
    EscalationPolicy::with_terminal(rules, EscalationAction::Skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_3_levels_with_terminal() {
        let policy = default_escalation_policy();
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::RetryWithComment);
        assert_eq!(policy.resolve(3), EscalationAction::Hitl);
        assert_eq!(policy.terminal_action(), Some(&EscalationAction::Skip),);
    }

    #[test]
    fn resolve_beyond_max_uses_highest_rule() {
        let policy = default_escalation_policy();
        // 가장 높은 숫자 키(3)의 Hitl이 반환된다.
        assert_eq!(policy.resolve(4), EscalationAction::Hitl);
        assert_eq!(policy.resolve(100), EscalationAction::Hitl);
    }

    #[test]
    fn empty_policy_returns_retry() {
        let policy = EscalationPolicy::default();
        assert!(policy.is_empty());
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.terminal_action(), None);
    }

    #[test]
    fn should_run_on_fail() {
        assert!(!EscalationAction::Retry.should_run_on_fail());
        assert!(EscalationAction::RetryWithComment.should_run_on_fail());
        assert!(EscalationAction::Hitl.should_run_on_fail());
    }

    #[test]
    fn is_retry() {
        assert!(EscalationAction::Retry.is_retry());
        assert!(EscalationAction::RetryWithComment.is_retry());
        assert!(!EscalationAction::Hitl.is_retry());
    }

    #[test]
    fn json_roundtrip() {
        let policy = default_escalation_policy();
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: EscalationPolicy = serde_json::from_str(&json).unwrap();
        for i in 0..=4 {
            assert_eq!(policy.resolve(i), parsed.resolve(i));
        }
        assert_eq!(policy.terminal_action(), parsed.terminal_action());
    }

    #[test]
    fn yaml_roundtrip() {
        let yaml = "1: retry\n2: retry_with_comment\n3: hitl\n";
        let policy: EscalationPolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::RetryWithComment);
        assert_eq!(policy.resolve(3), EscalationAction::Hitl);
        assert_eq!(policy.resolve(4), EscalationAction::Hitl);
        assert_eq!(policy.terminal_action(), None);
    }

    #[test]
    fn yaml_with_terminal() {
        let yaml = "1: retry\n2: retry_with_comment\n3: hitl\nterminal: skip\n";
        let policy: EscalationPolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::RetryWithComment);
        assert_eq!(policy.resolve(3), EscalationAction::Hitl);
        assert_eq!(policy.terminal_action(), Some(&EscalationAction::Skip),);
    }

    #[test]
    fn sparse_policy() {
        let mut rules = BTreeMap::new();
        rules.insert(1, EscalationAction::Retry);
        rules.insert(5, EscalationAction::Skip);
        let policy = EscalationPolicy::new(rules);

        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::Retry);
        assert_eq!(policy.resolve(5), EscalationAction::Skip);
        assert_eq!(policy.resolve(10), EscalationAction::Skip);
        assert_eq!(policy.terminal_action(), None);
    }
}
