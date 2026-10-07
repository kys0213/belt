# 알림·HITL spec 구현 (spec 대비 코드 차이 해소)

> **Plan — 이 시점의 결정 기록.** 현재 정책의 신뢰 소스가 아니다.
> 날짜: 2026-10-07 · 브랜치: epic/notification-hitl-impl · 이슈: 없음 (spec: PR #881, a7b19c0) · 승인 출처: council pass (생성·검증 agent 분리, 3라운드 + tie-break 자문 1회)

## 배경

- PR #881 로 main 의 spec 이 바뀌었다: SQLite 단일 상태 기준, 채널 무관 HITL first-wins, 실행 중 취소, NotificationChannel 분리, 스펙 기능 제거, 파생 아이템.
- 코드는 아직 이전 구조다. 아키텍트 협의체가 코드를 대조해 spec 과 다른 곳 41건(D-01~D-41)을 확정했다.
- 탐색으로 드러난 구조적 사실:
  - DB 에 마이그레이션 장치가 없다. 스키마가 `CREATE TABLE IF NOT EXISTS` 뿐이라 기존 DB 에 새 열이 반영되지 않고, first-wins 를 보장할 트랜잭션도 없다.
  - daemon 은 DB 를 선택적으로만 쓰고 메모리 큐가 사실상 기준이다. tick 이 handler 종료를 기다려 실행 중 취소를 받을 수 없다.
  - TUI 의 `x` 는 Scripts 탭이라 spec 의 취소 키와 겹친다.
  - DB 파서는 모르는 `hitl_reason` 을 만나면 에러를 낸다. 스펙 사유 값을 그냥 지우면 기존 행을 못 읽는다.

## 단계

각 단계는 epic 에 단독 머지되고, 머지 뒤 `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` 가 green 이어야 다음 단계로 간다.

| 단계 | 목적 |
|------|------|
| P1 | 스펙 기능 코드 제거 (스펙용 HITL 사유 값은 데이터 이관 뒤 P5 끝에 제거) |
| P2 | core 전이·계열 정책, 설정 검증, DB 마이그레이션 장치와 v2 스키마, 저장소 API |
| P3 | daemon·CLI 의 상태 변경을 저장소 전이 계약으로, 수집·발번·재시작 복원 |
| P4 | 실패 처리 순서, 파생 아이템과 worktree 인계, 실패 횟수 리셋, 의존 gate 계열 판정 |
| P5 | HITL 단일 계약과 daemon 후처리, timeout 경합, CLI·TUI 응답 |
| P6 | 실행 중 취소 (handler 프로세스 식별·종료, 비차단 실행, 재시작 종결) |
| P7 | NotificationChannel, GitHub origin channel, 응답 수신·allowlist·자연어 제안 |

- P5 머지 전에는 epic 을 main 에 머지하거나 릴리스하지 않는다. P2 마이그레이션 뒤 일부 구간에서 HITL 요청 행이 비고, P7 전까지 GitHub 코멘트가 나가지 않는다.
- `#[deprecated]` 는 쓰지 않는다. clippy `-D warnings` 가 deprecated 경고를 에러로 만들어 중간 단계가 red 가 된다.
- 같은 단계의 병렬 task 는 파일 집합이 겹치지 않고 컴파일 의존이 없을 때만 둔다. daemon.rs·main.rs 처럼 큰 파일을 고치는 task 는 순차로 두고 새 로직은 신규 모듈로 뺀다.

## 결정과 근거

- **기존 DB 업그레이드**: `PRAGMA user_version` 으로 앞으로만 마이그레이션하고 열기 전에 파일을 백업한다. 테이블·열은 지우지 않는다. 구버전 바이너리로 되돌리는 것은 지원하지 않는다 (가정, 0.1.x 이고 릴리스 노트로 알린다).
- **스펙용 HITL 레거시 데이터**:
  - `spec_conflict` 는 실제 이슈 아이템이라 `manual_escalation` 사유의 열린 요청으로 옮긴다.
  - `spec_completion_review`(가짜 아이템)와 `spec_modification_proposed`(replan 동반 아이템)는 응답 의미가 없어 hook 없이 Skipped 로 닫는다.
  - 버린 안: 셋 다 열린 요청으로 이관. done 응답이 엉뚱한 on_done(PR 생성 등)을 실행하거나, 같은 출처·state 에 활성 아이템이 둘 생긴다.
- **아이템별 terminal action**: skip·replan 이 아닌 저장값은 비워서 workspace 설정을 따르게 하고 원래 값은 메모에 남긴다. 추측해서 바꾸면 운영자 의도가 바뀐다.
- **기존 work_id**: 계열의 첫 아이템으로 두고 다음 순번은 최대값 다음부터 매긴다.
- **기존 설정**: terminal 이 없거나 레벨 값에 skip·replan 을 쓴 workspace 는 로드에 실패하고, 오류 메시지에 고칠 키와 허용 값을 적는다 (spec 의 Fail Fast).
- **정책 위치**: 재수집 판정·순번 발번·실패 횟수 리셋 집계는 core 순수 함수로 두고 infra 는 트랜잭션 안에서 호출만 한다.
- **P3 의 escalation 반영**: P3 에서 escalation 결과(retry·skip·hitl)를 임시로 DB 에 반영하고 P4 에서 파생 규칙으로 바꾼다.
  - 버린 안: DB 동기화를 P4 뒤로 미루기. 메모리에서만 Hitl 이 된 아이템이 DB 에 Running 으로 남아, 재시작 복원이 Pending 으로 되돌리면 HITL 이 사라지고 아이템이 다시 실행된다.
- **pid 보고**: 기존 `execute`·`invoke`·`RuntimeRequest` 는 그대로 두고 sink 를 별도 인자로 받는 `*_with_sink` 를 추가한다. 모든 내장 런타임이 이를 구현해 pid 를 보고하는지 테스트로 강제한다.
  - 버린 안: `RuntimeRequest` 에 필드 추가. CLI 의 literal 이 깨지고 `Default` 는 빈 값을 조용히 채운다.
- **pid 가 없는 취소**: spawn 전이면 spawn 하지 않고 `canceled`, daemon 부재 중 CLI 직접 경로면 `canceled_directly`. spec 에 없는 결과 값을 만들지 않는다.
- **`belt context` 의 `derived_from`**: serde default 로 추가하고 `Default` derive 는 쓰지 않는다.

## 가정

- evaluate 실패 횟수는 메모리에 두고, HITL retry 후처리에서 0 으로 되돌리며, 재시작 뒤에는 0 이다. spec 이 정하지 않은 부분이고, 사용자의 "retry 뒤 1단계부터" 결정과 같은 방향이다.
- 상한 값은 spec 이 구현에 맡겼다: 후처리 실패 5회, 전달 재시도 5회, CLI 취소 대기 10초, SQLite busy 대기 5초.
- TUI Scripts 탭 단축키를 `x` 에서 `t` 로 옮긴다.
- GitHub 명시 응답 형식은 `/belt <done|retry|skip|replan> [hitl_id]`, 그 밖은 자연어로 본다.
- 자연어 해석은 `runtime.default` AgentRuntime 이 맡고, 응답자·HITL 당 pending 제안은 하나다.
- spec 의 "daemon 로그 정리"는 daemon 이 로그 파일을 만들지 않아 대상이 없다. log-cleanup 은 worktree 를 소유 아이템 기준으로 정리하는 것만 고친다.
- `specs`·`spec_links` 테이블은 기존 DB 에서 지우지 않고 새 DB 에서만 만들지 않는다.

## 위험

- daemon.rs(6천 줄 이상) 집중 수정 → 단계 안에서 순차, 새 로직은 신규 모듈.
- 두 프로세스의 SQLite 경합 → WAL, busy_timeout, `BEGIN IMMEDIATE`, 파일 DB 경합 테스트.
- 설정 Fail Fast 로 업그레이드 직후 시작 실패 → 오류 메시지에 허용 값, 릴리스 노트.
- handler 비차단 실행 전환이 concurrency·graceful shutdown 을 깨뜨릴 수 있음 → 회귀 테스트 포함.

## 범위 밖

- Discord 등 새 외부 channel 구현. spec 의 NotificationChannel 추상과 기본 channel 만 만든다.
- epic 의 main 머지. 이 런은 PR 생성까지만 한다.
