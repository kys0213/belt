---
paths:
  - "crates/belt-core/**/*.rs"
---

# Core 레이어 컨벤션 (도메인 모델 / trait / enum)

> 이 레이어에서 정의한 타입은 프로젝트 전체가 의존한다. 호출부만 보고 제약과 의도를 인지할 수 있어야 한다.

## 외부 의존성 최소화
- core에는 serde, thiserror, async-trait, chrono 수준만 허용한다.
- tokio, rusqlite 등 인프라 의존성을 core에 끌어오지 마라.

## 타입으로 제약을 표현하라
- 함수가 특정 enum variant만 허용한다면, enum 전체가 아닌 해당 variant 전용 struct를 파라미터로 받아라.
- `debug_assert`나 주석은 릴리스에서 무력화된다. 컴파일러가 잡을 수 있는 제약은 타입으로 표현한다.

## accessor는 Option을 반환하라
- 다형 컨테이너의 타입별 accessor가 silent default(`""`, `0`, `&[]`)를 반환하면 버그를 숨긴다.
- 해당 타입이 아닐 때 `None`을 반환하여 호출자가 명시적으로 처리하도록 강제한다.

## 상태 전이는 메서드로 캡슐화하라
- pub 필드 직접 변경으로 상태를 바꾸지 마라. 전이 의도를 표현하는 메서드를 제공한다.
- QueuePhase 전이는 반드시 `can_transition_to()`를 거쳐 유효성을 검증한다.

## 문자열 대신 enum을 사용하라
- 프로젝트에 이미 enum이 정의된 값을 `&str`로 받는 함수가 있으면 즉시 교체한다.

## clippy 경고는 suppress하지 말고 구조를 바꿔라
- `#[allow(clippy::too_many_arguments)]` 등은 문제를 숨기는 것이다.
- 파라미터가 7개를 넘으면 관련 필드를 struct로 추출한다.
