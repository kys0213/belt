# Release Recovery Runbook

릴리즈 파이프라인이 조용히 실패했을 때 상태를 진단하고 복구하는 절차를 기록한다. 이 런북은 2026-03–04 기간에 발생한 skip-labeling regression (v0.1.7–v0.1.10이 태그 없이 누적된 사건)의 복구 경험을 기반으로 작성되었다.

## 정상 상태 (Healthy Invariant)

다음 세 값이 모두 동일해야 한다:

- `.release-please-manifest.json` 의 `"."` 필드
- `Cargo.toml` 의 `workspace.package.version`
- `git tag --list 'v*' --sort=-v:refname | head -1` (접두사 `v` 제외)

유일한 허용 예외: `main` HEAD가 `chore(main): release X.Y.Z` 커밋이고, manifest가 최신 태그보다 정확히 한 단계 앞선 상태. 이는 Release PR이 머지된 직후 release-please가 태그를 찍는 짧은 윈도우다.

이외의 불일치는 모두 **조용한 실패**이며 CI의 `version-consistency` job이 즉시 감지한다.

## 방어 레이어 (Defense in Depth)

| 레이어 | 위치 | 역할 |
|--------|------|------|
| `verify-release` guard | `.github/workflows/release-please.yml` | HEAD가 Release PR 머지인데 `release_created=false`면 워크플로우 실패 |
| `version-consistency` job | `.github/workflows/ci.yml` | 모든 push/PR에서 manifest/Cargo/tag 3자 불일치 감지 |
| `push.tags: ['v*']` trigger | `.github/workflows/release.yml` | 수동/복구 태그 push도 자동 빌드로 연결 |
| binary smoke test | `.github/workflows/release.yml` | 업로드 전 각 네이티브 바이너리가 실행 가능하고 tag 버전과 일치하는지 확인 |
| Rust toolchain pin | `rust-toolchain.toml` | clippy/rustc 업그레이드는 계획된 PR로만 |

각 레이어가 다음 레이어의 입력을 신뢰하지 않는 것이 원칙이다.

## 증상별 진단

### 1. Release PR이 머지됐는데 태그가 없다

**원인 후보**
- release-please가 머지된 Release PR을 인식하지 못함 (skip-labeling, label 수동 제거, PR 본문 변조)
- release-please action의 권한 부족

**진단**
```bash
# 최근 release-please workflow run 로그에서 "No latest release pull request found" 검색
gh run list --workflow=release-please.yml --limit 5
gh run view <run-id> --log | grep -i "release pull request"
```

`release_created` output이 `false`라면 태그가 생성되지 않은 것이다. `verify-release` guard가 설치된 이후에는 이 상태에서 CI가 실패하므로, CI green인 경우 이 증상은 거의 발생하지 않는다.

**복구** → [태그 백필](#태그-백필)

### 2. 태그는 있는데 바이너리가 없다

**원인 후보**
- `gh release create`로 태그를 만든 경우 (`GITHUB_TOKEN`이 찍은 태그는 workflow를 트리거하지 않는 GitHub Actions 정책)
- release.yml의 `push.tags` 트리거가 설치되기 전 생성된 태그 (#871 이전)
- 빌드는 성공했으나 smoke test에서 실패해 upload job이 스킵됨

**진단**
```bash
gh release view vX.Y.Z --json assets --jq '.assets | length'   # 0이면 바이너리 없음
gh run list --workflow=release.yml --limit 10                   # 해당 태그에 대한 실행 유무
```

**복구** → [바이너리 재빌드](#바이너리-재빌드)

### 3. Release PR이 반복적으로 새로 생성된다

**원인**: release-please가 이전 머지된 Release PR을 추적하지 못해 "다음 릴리즈"를 매번 처음부터 계산. 보통 `skip-labeling: true` 또는 라벨 제거와 함께 발생.

**진단**
```bash
# release-please-config.json에서 skip-labeling 확인
grep skip-labeling release-please-config.json
# 머지된 Release PR들에 autorelease 라벨이 붙어 있는지 확인
gh pr list --state merged --search "chore(main): release" --json number,labels --limit 5
```

**복구**: `skip-labeling: true`를 제거하고, 다음 Release PR부터 라벨이 정상적으로 붙도록 한다. 이미 누적된 유령 버전이 있다면 [태그 백필](#태그-백필)을 먼저 수행한다.

### 4. Release PR의 CHANGELOG에 중복/과거 버전이 섞여 있다

**원인**: PR이 생성된 시점의 "최신 태그"가 실제보다 과거로 잡혀 있었기 때문. 즉 태그 누락 상태에서 만들어진 PR은 오염되어 있다.

**복구**: 해당 PR을 close. 태그가 정상화된 이후 다음 `feat:`/`fix:` 커밋이 머지되면 release-please가 올바른 base에서 새 PR을 생성한다. 릴리즈 대상 커밋이 아직 없다면 기다리거나, 강제로 찍고 싶으면 `Release-As: X.Y.Z` footer가 담긴 빈 커밋을 push한다.

## 복구 절차

### 태그 백필

누락된 `vX.Y.Z` 태그를 해당 release commit에 찍고 GitHub Release를 생성한다.

1. **release commit SHA 확인**

   ```bash
   gh pr list --state merged --search "chore(main): release X.Y.Z" \
     --json mergeCommit --jq '.[0].mergeCommit.oid'
   ```

2. **CHANGELOG 섹션 추출**

   ```bash
   awk '/^## \[X\.Y\.Z\]/,/^## \[/' CHANGELOG.md | sed '$d' > /tmp/vX.Y.Z.md
   ```

3. **Release 생성** (태그가 함께 만들어진다)

   ```bash
   gh release create vX.Y.Z \
     --target <full-sha> \
     --title "vX.Y.Z" \
     --notes-file /tmp/vX.Y.Z.md
   ```

   `--target`에는 반드시 **full SHA**를 전달한다. 짧은 SHA는 `tag_name is not a valid tag` 오류를 낸다.

4. **바이너리 빌드** → [바이너리 재빌드](#바이너리-재빌드)

### 바이너리 재빌드

`gh release create`로 만든 태그는 workflow를 트리거하지 않으므로 명시적으로 dispatch한다.

```bash
gh workflow run release.yml -f tag_name=vX.Y.Z
```

대안: 로컬에서 태그를 만들고 push하면 `push.tags` 트리거가 발동한다 (단, 사용자 credential로 push해야 하며 `GITHUB_TOKEN`으로는 트리거되지 않는다).

```bash
git tag vX.Y.Z <full-sha>
git push origin vX.Y.Z
```

빌드 완료 후 검증:

```bash
gh release view vX.Y.Z --json assets --jq '.assets | length'   # 5여야 한다
```

### release-please 강제 재실행

```bash
gh workflow run release-please.yml
```

머지된 Release PR을 라벨링하거나 새 Release PR을 리프레시할 때 유용하다. 단 release-worthy 커밋(`feat:`, `fix:`, `refactor:`, `docs:` 등)이 없으면 새 PR은 만들어지지 않는다.

## 원칙

- **가드를 끄지 않는다.** `version-consistency` 실패는 본인이 마지막 수정자가 아니더라도 반드시 원인을 파악한다. 무시하면 그 순간 방어망이 뚫린다.
- **릴리즈 commit에 수동 커밋을 얹지 않는다.** `chore(main): release` commit 위에 직접 수정을 올리면 `verify-release` guard의 HEAD 인식이 깨진다. 필요한 수정은 별도 PR로.
- **`GITHUB_TOKEN`의 한계를 기억한다.** 이 토큰이 만든 태그/commit은 다른 workflow를 트리거하지 않는다. 복구 작업은 사용자 credential 또는 `workflow_dispatch`로.
- **skip-labeling은 쓰지 않는다.** 라벨이 release-please의 유일한 "이 PR은 내가 만든 것" 식별 수단이다.
