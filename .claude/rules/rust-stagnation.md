---
paths:
  - "**/stagnation/**/*.rs"
---

# Stagnation 모듈 컨벤션

> 에이전트 루프 감지와 사고 전환 로직. 합성(Composite) 패턴으로 구성하고, Persona는 서브모듈에 격리한다.

## 원칙

1. **합성으로 조합하라**: SimilarityJudge와 PatternDetector는 항상 Composite를 통해 사용한다. 단일 구현을 직접 호출하면 확장 시 수정이 필요해진다.
2. **빠른 실패 우선**: PatternDetector 체인 순서는 O(1) 검사 먼저. ExactHash → TokenFingerprint → NcdJudge 순서로 등록한다.
3. **Persona 격리**: Persona 정의와 프롬프트 템플릿(`personas/*.md`)은 `lateral.rs` 내부에만 존재한다. core 외부에 노출하지 않는다.
4. **결과는 LateralPlan으로 변환**: 분석 결과를 raw 문자열로 상위에 전달하지 않는다. `LateralPlan` 구조체로 변환 후 전달한다.

## DO

```rust
// SimilarityJudge는 CompositeSimilarity로 조합한다
let judge = CompositeSimilarity::new(vec![
    Box::new(ExactHash),
    Box::new(TokenFingerprint),
    Box::new(NcdJudge::default()),
]);

// PatternDetector도 StagnationDetector로 합성한다 (빠른 실패 먼저)
let detector = StagnationDetector::new(vec![
    Box::new(SpinningDetector::new(Box::new(ExactHash), 0.9, 2)),  // O(1) — 먼저
    Box::new(OscillationDetector::new(Box::new(TokenFingerprint), 0.9, 2)),
]);

// LateralPlan으로 변환하여 상위에 전달
let plan: LateralPlan = analyzer.analyze(executor, &params).await?;
```

```rust
// Persona는 lateral 모듈 내부에서만 구성한다
// pub use lateral::{LateralAnalyzer, LateralPlan, Persona};  ← mod.rs re-export만 허용
```

## DON'T

```rust
// 단일 SimilarityJudge 구현을 직접 사용하지 않는다
let judge = TokenFingerprint;  // 나쁨 — 나중에 NCD 추가 시 호출부 수정 필요
let score = judge.score(a, b);

// NcdJudge를 먼저 등록하지 않는다 — 압축 연산은 비싸다
let detector = StagnationDetector::new(vec![
    Box::new(NcdJudge::default()),   // 나쁨 — O(n log n) 연산이 먼저
    Box::new(ExactHash),
]);

// Persona 구성 로직을 lateral 모듈 바깥에 두지 않는다
let persona = Persona::Hacker;
let prompt = persona.prompt_template();  // 나쁨 — 외부 코드가 prompt 조립을 담당
```

## 체크리스트

- [ ] SimilarityJudge를 직접 사용하는 곳이 없고 CompositeSimilarity를 통하는가
- [ ] PatternDetector 등록 순서가 O(1) 검사(ExactHash) 먼저인가
- [ ] Persona 구성과 프롬프트 빌드가 `lateral.rs` 내부에만 있는가
- [ ] 상위 레이어에 `StagnationDetection`이 아닌 `LateralPlan`을 전달하는가
- [ ] `personas/` 서브모듈의 `.md` 파일이 `include_str!`로만 참조되는가
