//! 계열(lineage) 정책 — 재수집 판정, 순번 발번, 실패 횟수 리셋 집계.
//!
//! 저장소는 트랜잭션 안에서 현재 기록을 읽어 이 순수 함수를 호출만 한다.

use crate::phase::QueuePhase;

/// 재수집 판정 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectDecision {
    /// 같은 `(source_id, state)`에 종결되지 않은 아이템이 있어 수집하지 않는다.
    Duplicate,
    /// 새 아이템을 만든다. `seq`가 `None`이면 첫 아이템(`{source_id}:{state}`), 아니면 `:{n}`.
    New { seq: Option<u32> },
}

/// 같은 `(source_id, state)`의 기존 아이템 phase로 재수집 여부와 순번을 판정한다.
///
/// Done·Skipped가 아닌 아이템이 하나라도 있으면(Failed 포함) `Duplicate`다.
/// `max_seq`는 지금까지 발번된 최대 `n`이며, 첫 아이템 뒤에는 최소 2부터 시작한다.
pub fn collect_decision(existing: &[QueuePhase], max_seq: Option<u32>) -> CollectDecision {
    let has_open = existing.iter().any(|phase| match phase {
        QueuePhase::Done | QueuePhase::Skipped => false,
        QueuePhase::Pending
        | QueuePhase::Ready
        | QueuePhase::Running
        | QueuePhase::Completed
        | QueuePhase::Hitl
        | QueuePhase::Failed => true,
    });
    if has_open {
        return CollectDecision::Duplicate;
    }
    if existing.is_empty() && max_seq.is_none() {
        return CollectDecision::New { seq: None };
    }
    let next = max_seq.map_or(2, |max| max.saturating_add(1).max(2));
    CollectDecision::New { seq: Some(next) }
}

/// 계열의 시도 이력 한 건의 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus {
    Running,
    Done,
    Failed,
    Skipped,
    Hitl,
    /// 리셋 지점 — HITL retry 확정, replan 파생, 새 계열 시작.
    Reset,
}

/// 마지막 리셋 지점 이후의 실패 수 (`failure_count`).
pub fn count_since_reset(attempts: &[AttemptStatus]) -> u32 {
    let start = attempts
        .iter()
        .rposition(|a| *a == AttemptStatus::Reset)
        .map_or(0, |i| i + 1);
    let failed = attempts[start..]
        .iter()
        .filter(|a| **a == AttemptStatus::Failed)
        .count();
    u32::try_from(failed).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use QueuePhase::*;

    #[test]
    fn open_item_is_duplicate() {
        for open in [Pending, Ready, Running, Completed, Hitl] {
            assert_eq!(
                collect_decision(&[Done, open], Some(2)),
                CollectDecision::Duplicate
            );
        }
    }

    #[test]
    fn failed_only_is_duplicate() {
        assert_eq!(
            collect_decision(&[Failed], None),
            CollectDecision::Duplicate
        );
    }

    #[test]
    fn nothing_existing_is_first_item() {
        assert_eq!(
            collect_decision(&[], None),
            CollectDecision::New { seq: None }
        );
    }

    #[test]
    fn terminal_only_takes_next_after_max() {
        assert_eq!(
            collect_decision(&[Done, Skipped, Done], Some(3)),
            CollectDecision::New { seq: Some(4) }
        );
    }

    #[test]
    fn terminal_without_max_starts_at_two() {
        assert_eq!(
            collect_decision(&[Done], None),
            CollectDecision::New { seq: Some(2) }
        );
    }

    #[test]
    fn seq_is_never_below_two() {
        assert_eq!(
            collect_decision(&[Done], Some(1)),
            CollectDecision::New { seq: Some(2) }
        );
    }

    #[test]
    fn counts_failures_without_reset() {
        use AttemptStatus::*;
        assert_eq!(count_since_reset(&[Failed, Skipped, Failed]), 2);
        assert_eq!(count_since_reset(&[]), 0);
    }

    #[test]
    fn counts_only_after_last_reset() {
        use AttemptStatus::*;
        assert_eq!(
            count_since_reset(&[Failed, Failed, Reset, Failed, Reset, Running, Failed]),
            1
        );
        assert_eq!(count_since_reset(&[Failed, Reset]), 0);
    }

    #[test]
    fn non_failed_attempts_are_not_counted() {
        use AttemptStatus::*;
        assert_eq!(count_since_reset(&[Running, Done, Hitl, Skipped]), 0);
    }
}
