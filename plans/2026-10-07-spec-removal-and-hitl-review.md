# 스펙 기능 제거와 알림·HITL spec 재검토 반영

> **Plan — 이 시점의 결정 기록.** 현재 정책의 신뢰 소스가 아니다.
> 날짜: 2026-10-07 · 브랜치: spec-notification-hitl · 이슈: 없음 (PR #881) · 승인 출처: council pass (사용자 결정 U1~U11 입력)

## 배경

- PR #881 은 알림·HITL 을 출처 DataSource 와 내부 대시보드 양쪽에서 다루도록 spec 만 개정했다 (`plans/2026-10-04-notification-hitl-spec.md`).
- 개정본을 세 관점(문서 간 정합성, spec 작성 규칙, 요구·운영 시나리오 커버리지)으로 다시 검토했고 30건 가까운 지적이 나왔다. 그중 6건은 같은 spec 을 보고 다르게 구현할 수 있는 모순이었다.
  - 실패 아이템의 수동 done 허용 여부, replan 결과, escalation retry 의 "새 아이템" 식별자, 만료 시 worktree 정리, 실패 처리 순서, 수락된 취소 요청의 crash 후 잔존.
- 검토 중 사용자가 "belt 에 스펙 기능이 남아 있으면 제거하고, 이벤트 소스별 파이프라인 도구로만 가져가고 싶다"고 정했다. 스펙 HITL(완료 확인·충돌)의 어휘 불일치는 이 결정으로 자연히 사라진다.

## 사용자 결정

| ID | 결정 |
|----|------|
| U1 | 스펙 기능(스펙 등록·분석·이슈 분해, gap-detection, 스펙 충돌 gate, 스펙 완료 확인, 스펙 수정 제안, `belt spec`, `/spec`)을 제거한다. 이번 PR 에서는 spec 문서에서만 걷어내고 코드 제거는 후속으로 둔다. 큐 아이템 간 선후 의존(`belt queue dependency`)은 유지한다 |
| U2 | replan(한도 3회 이내)은 원 아이템을 Skipped 로 끝내고, 같은 출처로 새 work_id 를 받은 아이템을 원 아이템과 연결해 만든다. 새 아이템은 실패 맥락을 들고 계획부터 다시 세운다. 한도 초과는 Failed(worktree 보존) |
| U3 | 새 아이템을 만드는 모든 경로는 work_id 를 새로 발번하고 원 아이템과 연결한다 |
| U4 | HITL retry 뒤 다시 실패하면 실패 횟수를 리셋하고 escalation 1단계부터 다시 적용한다 (분기 최소화) |
| U5 | Failed → Done 수동 전이는 없다 |
| U6 | worktree 는 원칙대로 Done·Skipped 일 때만 정리한다. HITL 만료도 예외가 아니다 |
| U7 | 상태 전이를 먼저 기록하고, 기록이 적용됐을 때만 hook 을 부른다. hook 실패는 상태를 되돌리지 않는다 |
| U8 | daemon 재시작 시 열린 취소 요청을 종결하는 단계를 둔다 |
| U9 | `belt:needs-human` 라벨은 모든 HITL 열기에서 붙인다 ("HITL 열림" hook 시점 추가) |
| U10 | `notifications` 설정은 daemon 재시작 시 반영한다 |
| U11 | HITL 액션 어휘는 done/retry/skip/replan 하나로 통일한다 |

사용자는 이어서 자율 진행을 지시했다. 아래 메인 기본값(D1~D8)과 세부 계약은 아키텍트 협의체(생성·검증 agent 분리, 3라운드, tie-break 자문 1회)를 거쳐 확정했다.

## 결정과 근거

### 아이템 식별과 파생

- (source_id, state) 에서 처음 만든 아이템은 `{source_id}:{state}`, 이후 아이템은 파생이든 재수집이든 `{source_id}:{state}:{n}` 이다. n 은 (source_id, state) 단위로 2부터 단조 증가하고 work_id 는 재사용하지 않는다.
  - 근거: U3. 처음엔 파생 아이템에만 n 을 붙였지만, changes-requested 루프처럼 같은 출처·state 가 다시 수집되는 기존 흐름과 충돌했다.
- escalation retry 와 replan 은 "파생 아이템"을 만들고 파생 원본을 기록한다. 같은 최초 아이템에서 이어진 아이템들을 "계열"이라 부른다. HITL retry 는 파생이 아니라 같은 아이템이 Pending 으로 돌아간다.
- escalation retry 의 원 아이템은 Skipped(파생됨)로 끝나고 worktree 는 파생 아이템에 인계된다.
  - 근거: U2·U3 와 같은 규칙. Failed 로 두면 Failed TTL 정리가 인계한 worktree 를 지운다.
- replan 파생 아이템은 원 아이템의 state 에서 시작한다.
  - 근거: daemon 은 도메인 로직을 모르고, state 진입은 yaml trigger 가 정한다. daemon 이 "계획 state" 를 고를 수 없다.
- 파생으로 끝난 Skipped 는 `skipped` 알림을 내지 않는다. 작업이 파생 아이템에서 이어지기 때문이다.
- 재수집은 같은 (source_id, state) 의 모든 아이템이 Done·Skipped 일 때만 새 계열을 만든다. Failed 가 남아 있으면 재수집하지 않고, 운영자가 skip 으로 정리해야 새 계열이 시작된다.
  - 근거: 처음 설계는 Failed 를 종결로 봤다. 그러면 라벨이 남은 Failed 가 다음 tick 에 새 계열로 다시 수집되고 실패 횟수·replan 횟수가 0 부터 다시 세져 U2 의 한도 초과 → Failed 가 무력해진다. 기존 spec 에서도 Failed 는 종결 상태가 아니다.
- 큐 의존 gate 는 선행 아이템 계열의 최신 아이템 phase 로 판정한다.
  - 근거: 원 아이템만 보면 retry 한 번으로 후행 아이템이 영구히 막힌다.

### escalation

- 실패 횟수는 마지막 리셋 지점(HITL retry 확정, replan 파생) 이후의 실패 수다. replan 파생을 리셋 지점에 넣은 것은 가정이다 — "계획부터 다시" 라는 U2 와 맞고, 누적하면 정의되지 않은 4회차 이후로 바로 빠진다.
- replan 한도 3회는 계열 전체에서 센다. 파생마다 세면 한도가 무력해진다.
- 레벨 값은 retry·retry_with_comment·hitl, terminal 은 skip·replan 만 받고 그 밖의 값은 설정 로드 시 거부한다 (D1, Fail Fast). 실행 중 "해석 불가 → Failed" 분기는 없앤다.
- 실패 횟수가 정의된 최고 레벨을 넘으면 최고 레벨을 재사용한다. 지금 코드 동작과 같다.
- `Running → Failed` 는 escalation 대상이 아닌 실패(인프라 오류, 예를 들어 worktree 생성 실패)를 뜻한다.
  - 버린 안: "대응하는 레벨이 없음" 으로 정의하기. 코드에 그런 상황이 없고, 실제 Running → Failed 경로(인프라 오류)가 spec 에서 사라진다.

### 실패 처리와 HITL

- 실패 처리 순서: escalation 결정 → 결과 전이 기록 → 적용됐을 때만 `on_escalation`, retry 가 아니면 `on_fail` (U7).
- `on_hitl_opened` 를 추가한다. daemon 이 열린 HITL 요청을 관찰하면 경로와 무관하게 한 번 호출하고, GitHub 구현은 `belt:needs-human` 라벨을 붙인다. 실패해도 재시도하지 않는다 (표시 용도라 단순함을 택한 가정).
- on_done 이 실패해 Failed 로 갈 때도 `on_hitl_resolved` 를 불러 라벨을 지운다 (D2).
- 아이템당 열린 HITL 요청은 하나이고 hitl_id 는 전역 고유다 (D3).
- HITL 만료도 worktree 예외가 없다. terminal skip 은 Skipped(정리), terminal replan 은 replan 규칙 그대로다 (U6).
- 자연어 제안은 DB 에 기록하고, HITL 이 확정되면 종결된다. 종결 뒤 온 확인은 `already_handled` 로 회신한다 (D6 보정).
  - 버린 안: `proposal_expired` 값 유지. 확정 뒤 확인이라는 같은 상황에 두 값이 생긴다.

### 취소·조회·보관·설정

- daemon 재시작 시 handler 를 정리한 뒤, 열린 취소 요청을 Running 롤백보다 먼저 종결한다. 대상이 Running 이면 Skipped + `canceled`, 아니면 `too_late`. daemon 이 수락했지만 종결이 늦으면 CLI 는 `accepted`(exit 0) 로 끝난다 (U8).
- `belt queue show <work_id>` 가 거절 기록과 파생 원본을 포함한 전이 이력을 보여준다. allowlist 밖 응답도 여기서만 확인한다 (D4).
- daemon 이 꺼진 동안 외부 채널로 온 응답은 재시작 후 polling 이 소급 수신한다 (D5).
- 전이 이력·HITL 요청·응답 판정·중복 제거 키·취소 요청·자연어 제안은 log-cleanup 대상이 아니다. log-cleanup 은 보존 worktree 와 daemon 로그만 정리한다 (D7). DB 가 계속 커지는 것은 운영 이슈로 남긴다.
- `notifications` 는 재시작 시 반영되고, hook 선택 설정은 기존대로 다음 트리거에 반영된다 (U10).
- 전이 계약 결과는 `applied`·`busy`·`conflict`·`invalid_action` 네 값이다.

### 스펙 기능 제거 범위

- 큐 HITL 은 Running(및 evaluate)·수동 요청에서만 열린다. `Ready → Hitl`(스펙 충돌)과 `[*] → Hitl`(replan·스펙 완료 직접 생성) 간선은 없어진다.
- HITL 사유는 `evaluate_failure`·`retry_max_exceeded`·`timeout`·`manual_escalation`·`stagnation_detected` 다.
- `flows/02-spec-lifecycle.md` 는 archive 로 옮기지 않고 삭제한다. archive 는 버전 단위 스냅샷이고 v5 스냅샷에 같은 flow 가 이미 있어, v6 파일 하나만 옮기면 스냅샷 단위가 깨진다.
- `belt queue retry-script` 는 성공 시 Failed → Done 을 만들어 U5 와 충돌하므로 spec 에서 없앤다.

### 작성 규칙 정리

- lifecycle-hook mermaid 메시지 안의 `;` 를 없앤다. GitHub 렌더링이 깨질 수 있다.
- lifecycle-hook·notification 의 "왜 분리했나·과거 문제" 서술과 lifecycle-hook "영향 범위" 절은 결정 경위라 spec 에서 지운다. 경위는 2026-10-04 plan 에 이미 있다.
- spec 이 plan 을 링크하는 곳(DESIGN, README, data-model)과 datasource 의 미래 서술을 정리한다.

## 범위 밖

- 코드 변경 전부. 스펙 기능 제거, 파생 발번, (source_id, state) 단위 재수집 판정(현재 코드는 같은 work_id 가 큐에 있는지만 본다), 레벨·terminal 값 거부, retry-script, on_hitl_opened, 재시작 시 취소 종결.
- main 에 원래 있던 작성 규칙 부채: DESIGN 의 Tick 순서·내부 구조, flows/05 의 Phase 문구, stagnation 의 plan 링크 5곳, cross-platform 의 구현 파일명 표.
- 레벨이 retry 뿐인 workspace 는 최고 레벨 재사용으로 파생이 끝없이 이어질 수 있다. 지금 코드 동작이라 코드 PR 에서 검토할 후보로만 남긴다.
- D1 로 다른 terminal·레벨 값을 쓰던 기존 설정은 업그레이드 뒤 로드에 실패한다. 코드 PR 의 릴리스 노트 대상이다.

## 작업 방식

- 문서 22개를 파일 집합이 겹치지 않는 7묶음으로 나눠 병렬로 고친다: 상태 머신·escalation / 기록 계약·설정 스키마 / 실행 루프·cron / hook·channel / CLI·에이전트 / 설계 개요와 앞쪽 flow / 실패·HITL·모니터링 flow.
- 묶음이 같은 개념을 다르게 쓰지 않도록 공통 계약 문구를 함께 준다.
- 합친 뒤에는 작성자와 다른 agent 가 문서 간 정합성과 spec 작성 규칙 두 관점을 검토한다.
