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
#[derive(Debug, Clone, Default)]
pub struct EscalationPolicy {
    rules: BTreeMap<u32, EscalationAction>,
}

impl Serialize for EscalationPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.rules.len()))?;
        for (k, v) in &self.rules {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for EscalationPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: BTreeMap<String, EscalationAction> = BTreeMap::deserialize(deserializer)?;
        let mut rules = BTreeMap::new();
        for (k, v) in raw {
            let key: u32 = k
                .parse()
                .map_err(|_| serde::de::Error::custom(format!("invalid escalation key: {k}")))?;
            rules.insert(key, v);
        }
        Ok(Self { rules })
    }
}

impl EscalationPolicy {
    pub fn new(rules: BTreeMap<u32, EscalationAction>) -> Self {
        Self { rules }
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

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// 기본 5단계 escalation 정책.
pub fn default_escalation_policy() -> EscalationPolicy {
    let mut rules = BTreeMap::new();
    rules.insert(1, EscalationAction::Retry);
    rules.insert(2, EscalationAction::RetryWithComment);
    rules.insert(3, EscalationAction::Hitl);
    rules.insert(4, EscalationAction::Skip);
    rules.insert(5, EscalationAction::Replan);
    EscalationPolicy::new(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_5_levels() {
        let policy = default_escalation_policy();
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::RetryWithComment);
        assert_eq!(policy.resolve(3), EscalationAction::Hitl);
        assert_eq!(policy.resolve(4), EscalationAction::Skip);
        assert_eq!(policy.resolve(5), EscalationAction::Replan);
    }

    #[test]
    fn resolve_beyond_max_uses_highest_rule() {
        let policy = default_escalation_policy();
        assert_eq!(policy.resolve(6), EscalationAction::Replan);
        assert_eq!(policy.resolve(100), EscalationAction::Replan);
    }

    #[test]
    fn empty_policy_returns_retry() {
        let policy = EscalationPolicy::default();
        assert!(policy.is_empty());
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
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
        for i in 0..=6 {
            assert_eq!(policy.resolve(i), parsed.resolve(i));
        }
    }

    #[test]
    fn yaml_roundtrip() {
        let yaml = "1: retry\n2: retry_with_comment\n3: hitl\n";
        let policy: EscalationPolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.resolve(1), EscalationAction::Retry);
        assert_eq!(policy.resolve(2), EscalationAction::RetryWithComment);
        assert_eq!(policy.resolve(3), EscalationAction::Hitl);
        assert_eq!(policy.resolve(4), EscalationAction::Hitl);
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
    }
}
