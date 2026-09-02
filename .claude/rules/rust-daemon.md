---
paths:
  - "**/belt-daemon/**/*.rs"
---

# Daemon 레이어 컨벤션 (실행 루프 + Cron)

> Daemon은 상태 머신 + yaml에 정의된 prompt/script를 호출하는 단순 실행기다. 도메인 로직을 모른다.

## Daemon이 알아야 하는 것
- 큐 아이템의 phase 상태 머신과 그 전이
- yaml에 정의된 prompt/script 실행
- Concurrency 제한 (workspace + global 2단계)

## Daemon이 몰라야 하는 것
- GitHub 라벨, PR, 이슈 번호 등 DataSource 도메인
- LLM 모델 선택 로직 (AgentRuntime이 담당)
- 외부 시스템 반영 방법 (on_done/on_fail script가 담당)

## match는 exhaustive하게 작성하라
- phase 상태를 나타내는 enum에 match할 때는 wildcard(`_`) 대신 모든 variant를 명시적으로 나열한다.
- 새 variant가 추가되면 컴파일 에러로 누락 지점이 드러나야 한다.

## core가 제공하는 전이 메서드를 사용하라
- 아이템의 phase 필드를 직접 변경하지 마라.
- core가 제공하는 전이 검증 메서드를 거쳐, 정의되지 않은 전이가 걸러지게 한다.

## Graceful Shutdown
- Running 아이템 완료 대기 (timeout 30초)
- timeout 초과 시 Pending으로 롤백, worktree 보존
