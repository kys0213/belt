# 알림·HITL 채널과 큐 상태 소유권 spec 개정

> **Plan — 이 시점의 결정 기록.** 현재 정책의 신뢰 소스가 아니다.
> 날짜: 2026-10-04 · 브랜치: spec-notification-hitl · 이슈: 없음 · 승인 출처: 사용자 승인 (아키텍트 협의체 3라운드 + 확인 점검 pass 후)

---

## 배경

- 사용자가 belt의 기능 안정성 테스트를 준비하면서 Discord를 입력원 겸 알림·HITL 채널로 붙이는 것을 검토했다.
- 조사 결과, 진행 알림과 HITL에는 다음 공백이 있었다.
  - 알림을 내보내는 길은 GitHub 하나뿐이다 (`GitHubLifecycleHook`). hook은 workspace당 하나만 선택된다 (`crates/belt-daemon/src/hook_cache.rs:137-160`).
  - HITL 응답은 CLI로만 받는다. TUI는 보기 전용이고, 출처(GitHub 코멘트)에서 응답을 받는 경로가 없다.
  - daemon은 큐를 메모리에, CLI·cron은 DB를 기준으로 본다 (`crates/belt-daemon/src/daemon.rs:2093-2098`). 그래서 여러 경로의 응답 중 첫 응답만 반영된다는 보장을 할 수 없다.
- 이번 런은 spec 개정만 한다. 코드는 바꾸지 않는다.

## 요구사항 (사용자 확정)

1. 진행 알림과 HITL은 출처 채널과 내부 dashboard/CLI 양쪽에서 노출되고 응답할 수 있다. dashboard는 설정과 무관하게 항상 보인다.
2. 알림 경로는 설정으로 바꾼다. 출처 채널에 추가 채널을 fan-out하고, 설정이 없으면 출처로만 보낸다.
3. 여러 채널에서 응답이 오면 먼저 확정된 응답이 이긴다. 늦은 응답은 반영하지 않고 그 채널에 "이미 처리됨(누가/어디서/액션)"을 회신한다.
4. 큐 아이템과 아이템별 상태 변경 이력이 SQLite에 기록되고, 이것이 상태의 기준이다. 알림·HITL·dashboard는 이 위에서 동작한다.
5. 실행 중인 아이템은 즉시 취소할 수 있다.

## 사용자 결정

| 항목 | 결정 |
|------|------|
| 외부 채널 응답 권한 | 채널별 허용 목록. 비어 있으면 외부 응답을 받지 않는다 |
| 자연어 답변 | LLM이 액션을 제안하고 확인을 받은 뒤 반영한다. 경합에 참여하는 것은 확인된 액션이다 |
| 알림과 hook의 경계 | 사람에게 보내는 메시지는 새 `NotificationChannel`, 출처 상태 반영(라벨 등)은 `LifecycleHook` |
| 진행 알림 범위 | phase 단위 (started / done / failed / skipped / hitl_requested). 채널별로 고른다 |
| HITL 해결 시 라벨 | `belt:needs-human` 라벨을 제거한다 |
| 시작 알림 실패 | handler를 막지 않는다. 실패는 dashboard에 보인다 |
| 처리 중 변경 | handler 실행 중 / HITL 후처리 중인 아이템은 다른 경로의 상태 변경을 거절한다 (`busy`) |
| HITL done 확정 후 | 즉시 Done이 아니다. on_done 성공 시 Done, 실패 시 Failed |
| 실행 중 취소 | daemon 생존 시 취소 요청 + 즉시 깨움 → daemon이 handler 종료 후 Skipped. daemon 무응답 시 CLI가 직접 Skipped + 남은 handler 정리. 후처리 중에는 취소 불가 |
| spec 표현 | 개정 대상 문서 전체에서 DDL·trait 표·코드 수준 서술을 걷어내고, 전체 동작을 mermaid 다이어그램으로 표현한다 |

## 결정과 근거

- **SQLite가 큐 상태의 유일한 기준이다.** 모든 전이는 "처리 중 가드 → 조건부 phase 갱신 → 이력 추가"를 한 트랜잭션으로 하고, 결과는 `applied | busy | conflict` 값이다. daemon 메모리는 사본이고 tick마다 DB를 따른다.
  - 근거: 첫 응답 승리를 여러 프로세스(daemon, CLI, TUI, cron) 사이에서 보장하려면 판정 지점이 하나여야 한다. SQLite는 쓰기를 직렬화한다.
- **"처리 중"은 새 phase가 아니라 기존 상태로 판단한다.** Running이거나, Hitl이면서 응답은 확정됐지만 후처리가 끝나지 않은 경우다. 평가 중(Completed)은 잠그지 않는다. evaluator가 CLI로 전이하는 정당한 주체이기 때문이다.
- **HITL 요청을 아이템과 별개의 인스턴스로 다룬다.** 같은 `work_id`가 HITL에 재진입할 수 있어서(retry), 아이템 id로만 응답을 연결하면 이전 HITL에 대한 늦은 응답이 새 HITL을 닫는다.
- **HITL에서 나가는 전이는 daemon 후처리만 한다.** 다른 경로의 `queue skip/done`은 HITL 응답으로 바뀌어 경합에 참여한다. 그래서 "열린 HITL 요청이 있으면 아이템은 Hitl"이 항상 지켜진다.
- **후처리는 daemon이 단독으로, 최소 한 번 실행한다.** 아이템 생성과 spec 전이는 멱등이다. 결과 전이가 N회 연속 실패하면 Hitl→Failed로 빠져 영구 잠금을 막는다.
- **`NotificationChannel`을 분리한 주된 이유는 응답 수신(inbound)이다.** `LifecycleHook`은 쓰기 전용이고, 응답을 받는 반대 방향 책임을 넣으면 trait 의미가 무너진다. 두 어휘가 따로 자라지 않게 "전이 지점 → hook 콜백 / channel 이벤트" 매핑 표를 둔다.
- **진행 알림은 daemon이 이력을 읽어 best-effort로 보낸다.** HITL 요청 알림만 채널별 전달 기록을 두고 다음 tick에 재시도한다. 응답을 어느 메시지에 대한 것인지 연결하려면 채널별 메시지 참조가 필요하다.
- **외부 응답은 (채널, 외부 응답 id) 단위로 한 번만 처리한다.** polling이 같은 코멘트를 다시 읽어도 승자가 "이미 처리됨"을 받지 않는다.

## 버린 선택지

| 선택지 | 버린 이유 |
|--------|-----------|
| `LifecycleHook`을 여러 개 묶어 fan-out | 응답 수신 책임이 섞이고, hook마다 실패를 치명적으로 볼지가 갈린다 |
| 이력에서 상태를 재구성 (event sourcing) | 요구보다 크다. 상태는 테이블, 이력은 append 기록으로 충분하다 |
| 새 Processing phase / 잠금 컬럼 | 기존 상태로 판단할 수 있다 |
| 판정과 phase 전이를 한 트랜잭션으로 묶어 즉시 Done | on_done 실패를 처리할 길이 없다 (Done에서 나가는 전이 없음) |
| 해결 공지를 모든 채널에 fan-out | 요구 밖이다. 늦게 응답한 채널에만 회신하면 된다 |
| 전용 outbox + 재시도 백오프 | 단일 daemon·SQLite 규모에 과하다. HITL 요청 전달 기록과 tick 재시도로 충분하다 |
| 새 `belt queue cancel` 명령 | 최종 상태와 의도가 skip과 같다. `skip`을 phase별 의미로 재정의한다 |
| CLI가 항상 직접 취소 | 실행 중인 handler와 상태가 어긋나 충돌이 생긴다 |
| 외부 응답 권한을 저장소 write 권한으로 (자문 권고) | 사용자가 허용 목록을 골랐다 |
| 자연어는 고정 문법만 (자문 권고) | 사용자가 LLM 해석 + 확인을 골랐다 |

## 가정

- 응답 수신은 tick polling이다. daemon에 HTTP 서버가 없다.
- daemon은 한 DB에 하나만 돈다.
- daemon 무응답 판정은 "프로세스 부재, 또는 깨운 뒤 제한 시간 안에 취소 수락 기록 없음"이다. 제한 시간 값은 구현에서 정한다.
- 취소된 아이템의 worktree는 Skipped 규칙대로 정리한다.
- daemon이 꺼져 있던 동안의 전이는 진행 알림을 보내지 않는다. HITL 요청 알림은 재시작 후 보낸다.
- 평가 중 사람이 먼저 상태를 바꾸면 evaluator가 쓴 LLM 비용은 버려진다.
- daemon 시작 시 이전 daemon이 남긴 handler 프로세스를 먼저 정리한다.

## 구현 시 참고 (spec에 적지 않는 세부)

- HITL 요청 인스턴스 테이블 후보 필드: hitl_id, work_id, reason, notes, status(open/resolved/expired), resolved_action/by/via/at, timeout_at, terminal_action, post_processed_at, created_at. 기존 `queue_items`의 hitl_* 컬럼을 이 테이블로 옮긴다.
- 전이 이력은 기존 `transition_events`를 승격한다. phase 변경과 같은 트랜잭션, 전역 단조 순번(재사용 없음). 기존 id(`te-{work_id}-{millis}`)는 순서를 보장하지 않는다.
- handler 실행 중에도 daemon이 취소 요청을 받을 수 있어야 한다. 지금은 tick이 handler `JoinSet` 완료를 기다려 취소를 받지 못한다 (`daemon.rs:500-533`, `:1901-1909`).
- SIGUSR1은 지금 cron 동기화와 즉시 tick도 일으킨다. 취소 깨움과 의미가 겹친다.
- 동시 응답이 DB 잠금 에러로 끝나지 않게 한다 (`db.rs`에 트랜잭션·busy_timeout 설정이 없다).

## 현재 코드와의 차이 (구현 대상)

1. replan 상한 초과 시 spec은 Skipped, 코드는 Failed (`crates/belt-daemon/src/cron.rs:716`).
2. HITL retry 시 spec은 새 아이템, 코드는 같은 아이템을 Pending으로 되돌린다 (`crates/belt-daemon/src/hitl_service.rs:93-111`).
3. TUI HITL은 보기 전용이고, 안내 문구가 존재하지 않는 명령을 가리킨다 (`crates/belt-cli/src/dashboard.rs:2804`).
4. CLI 응답이 respondent를 기록하지 않는다. `db.respond_hitl`은 production caller가 없다.
5. timeout 기본값이 코드는 Failed다 (`cron.rs:620-623`).
6. 첫 응답 승리 보장이 없다 — 확인 후 무조건 갱신 (`crates/belt-cli/src/main.rs:3920-3927`, `crates/belt-infra/src/db.rs:407`).
7. daemon 메모리 큐가 사실상 기준이고 phase를 DB에 쓰지 않는다 (`daemon.rs:2093-2098`).
8. HITL 진입 지점이 7곳 이상 흩어져 있다.
9. 전이 이력이 phase 변경과 별도로 best-effort 기록된다 (`daemon.rs:300-325`, `crates/belt-daemon/src/advancer.rs:45-62`).
10. daemon이 시작 시 DB에서 큐를 복원하지 않는다.
11. CLI HITL done이 on_done을 부르지 않고 worktree를 직접 정리한다.
12. `belt queue done/skip`이 CLI 안에서 on_done·worktree 정리를 직접 실행한다 (`main.rs:1004-1040`).
13. `belt queue hitl/skip`이 처리 중 여부를 확인하지 않고 phase를 덮어쓴다 (`main.rs:1146-1170`).
14. 실행 중 취소 경로와 handler 프로세스 식별 정보 저장이 없다.

## 작업 순서

1. 기반 문서: `spec/concerns/queue-state-machine.md`, `data-model.md`, `daemon.md`, `cron-engine.md` — 상태 소유권, 전이 계약, 처리 중 잠금, 취소, 후처리
2. 알림·HITL 문서 (1과 병렬): 신규 `spec/concerns/notification.md`, `lifecycle-hook.md`, `datasource.md`, `workspace-schema.md`, `cli-reference.md`
3. 큰그림·시나리오 (1·2 이후): `spec/DESIGN.md`, `spec/flows/04-failure-and-hitl.md`, `spec/flows/05-monitoring.md`
4. 각 단계 산출물은 작성자와 다른 agent가 "설계 ↔ spec" 정합성으로 검토한다. 최종 HEAD에서 문서 간 참조 정합성을 확인하고 `cargo build`로 코드 무영향을 확인한다.

설계 원문(협의체 산출물)은 세션 기록에만 있고 repo에 남기지 않는다. 이 문서가 그 요지다.
