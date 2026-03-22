# Cron 엔진 — 주기 실행 + 품질 루프

> 주기적으로 실행되는 작업을 관리한다.
> 파이프라인은 1회성, 품질은 Cron이 지속 감시하여 새 아이템을 생성.

---

## 두 가지 역할

```
1. 인프라 유지 — hitl-timeout, log-cleanup, daily-report (결정적)
2. 품질 루프 — evaluate, gap-detection, knowledge-extract (LLM 사용)
```

---

## 품질 루프

파이프라인이 아이템을 처리한 후, Cron이 지속적으로 결과물을 검증한다.

```
Pipeline: issue → analyze → implement → review → Done
                                                   │
Cron: gap-detection ─── 스펙 vs 코드 비교 ──────────┘
        │
        ▼
      gap 발견 → 중복 검사 → 새 이슈 생성 → DataSource.collect() → 파이프라인 재진입
```

부족하면 되돌아가는 게 아니라 **새 아이템이 생긴다**.

### Dedupe 가드

gap-detection이 이슈를 생성하기 전, DataSource의 현재 open 아이템 목록을 조회하여 **동일 gap에 대한 아이템이 이미 존재하면 skip**한다.

```
gap 발견 → DataSource에서 open 아이템 조회 (Pending/Ready/Running)
  → 동일 gap에 해당하는 아이템 존재 → skip (이미 처리 중)
  → 해당 아이템 없음 → 새 이슈 생성
```

이를 통해 동일 문제에 대한 이슈 무한 증식을 방지한다.

---

## 기본 Cron Jobs

### 인프라 (Global, 결정적, 토큰 0)

| Job | 주기 | 동작 |
|-----|------|------|
| hitl-timeout | 5분 | 미응답 HITL 만료 처리 |
| daily-report | 매일 06시 | 일간 리포트 |
| log-cleanup | 매일 00시 | 오래된 로그/worktree 삭제 |

### 품질 루프 (Per-workspace, LLM 사용)

| Job | 주기 | 동작 |
|-----|------|------|
| evaluate | 60초 | 완료 아이템 분류 (Done or HITL) — `autodev agent -p` |
| gap-detection | 1시간 | 스펙-코드 대조, gap 발견 시 이슈 생성 |
| knowledge-extract | 1시간 | merged PR 지식 추출 |

### 사용자 정의 (예시)

| Job | 주기 | 동작 |
|-----|------|------|
| qa-test | 30분 | 테스트 실행, 실패 시 이슈 생성 |
| security-scan | 2시간 | 보안 취약점 스캔 |

---

## Force Trigger (하이브리드 실행 모델)

evaluate는 **cron 주기 폴링 + force_trigger 즉시 실행**의 하이브리드 모델로 동작한다:

```
1. 주기 폴링: evaluate cron이 60초마다 Completed 아이템을 스캔
2. 즉시 실행: handler 성공 → Completed 전이 → force_trigger("evaluate")
               → last_run_at = NULL → 다음 tick(10초 이내)에서 즉시 실행
```

- **force_trigger**는 동기적으로 evaluate를 실행하지 않는다. cron의 `last_run_at`을 리셋하여 다음 tick에서 우선 실행되도록 스케줄링한다.
- evaluate LLM 호출도 concurrency slot을 소비한다 (`daemon.max_concurrent`에 `active_evaluate_count`가 포함됨).
- 주기 폴링은 force_trigger가 누락된 경우(예: 프로세스 재시작)의 안전망 역할을 한다.

> handler 실패 시에는 force_trigger 없이 즉시 escalation 정책이 적용된다 (evaluate 불필요).

---

## 스크립트 구조

Cron job의 스크립트도 `autodev context`를 활용하여 필요한 정보를 조회한다.

```bash
#!/bin/bash
# Guard: Completed 상태 아이템 있을 때만
COMPLETED=$(autodev queue list --workspace "$WORKSPACE" --phase completed --json | jq 'length')
if [ "$COMPLETED" = "0" ]; then exit 0; fi

# 실행: evaluate
# LLM이 context를 조회하고 autodev queue done/hitl CLI를 직접 호출
autodev agent --workspace "$WORKSPACE" -p \
  "Completed 아이템의 완료 여부를 판단하고, autodev queue done 또는 autodev queue hitl 을 실행해줘"
```

---

## Daemon 주입 환경변수 (Cron 전용)

Cron 스크립트에는 workspace 정보가 필요하므로 추가 변수를 주입한다.

| 변수 | 예시 |
|------|------|
| `WORKSPACE` | `auth-project` |
| `AUTODEV_HOME` | `~/.autodev` |
| `AUTODEV_DB` | `~/.autodev/autodev.db` |

> **참고**: handler/on_done/on_fail script에는 `WORK_ID` + `WORKTREE`만 주입된다. Cron은 아이템 단위가 아니라 workspace 단위로 실행되므로 다른 환경변수 세트를 사용한다.

---

## Built-in vs Custom

| | Built-in | Custom |
|---|---|---|
| 생성 | workspace 등록 시 자동 | `autodev cron add` |
| 제거 | 불가 (pause/resume) | 자유 |
| Guard | 내장 | 사용자 정의 |

---

### 관련 문서

- [DESIGN-v5](../DESIGN-v5.md) — evaluate 아키텍처
- [DataSource](./datasource.md) — autodev context 스키마
- [Claw](./claw-workspace.md) — evaluate와 Claw의 관계
