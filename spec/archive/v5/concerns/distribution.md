# Distribution

Belt 바이너리를 사용자에게 쉽게 설치할 수 있는 경로를 제공한다.

## 설치 채널

| 채널 | 대상 | 명령 | 우선순위 |
|------|------|------|---------|
| Shell installer | macOS/Linux | `curl -sSf https://.../install.sh \| sh` | P0 |
| PowerShell installer | Windows | `irm https://.../install.ps1 \| iex` | P0 |

---

## 1. Shell Installer (`install.sh`)

macOS와 Linux 사용자를 위한 단일 스크립트.

### 동작

```
1. OS 감지 (Linux / Darwin)
2. 아키텍처 감지 (x86_64 / aarch64)
3. OS + Arch → GitHub Release 타겟 매핑
4. 최신 릴리즈 태그 조회 (GitHub API)
5. 바이너리 다운로드 + 검증
6. $INSTALL_DIR (기본 ~/.belt/bin) 에 설치
7. PATH 설정 안내 (shell profile 감지)
```

### 타겟 매핑

| OS | Arch | GitHub Release Asset |
|----|------|---------------------|
| Linux | x86_64 | `belt-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `belt-aarch64-unknown-linux-gnu.tar.gz` |
| Darwin | x86_64 | `belt-x86_64-apple-darwin.tar.gz` |
| Darwin | arm64 | `belt-aarch64-apple-darwin.tar.gz` |

### 환경 변수

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `BELT_INSTALL_DIR` | `~/.belt/bin` | 설치 경로 |
| `BELT_VERSION` | latest | 특정 버전 지정 (e.g. `v0.1.1`) |

### PATH 설정

스크립트는 설치 후 현재 셸 프로파일을 감지하여 PATH 추가 안내를 출력한다.

```
~/.zshrc    → export PATH="$HOME/.belt/bin:$PATH"
~/.bashrc   → export PATH="$HOME/.belt/bin:$PATH"
~/.profile  → export PATH="$HOME/.belt/bin:$PATH"
```

`--yes` 플래그가 주어지면 자동으로 프로파일에 추가한다.

### 의존성

- `curl` 또는 `wget` (다운로드)
- `tar` (아카이브 해제)

---

## 2. PowerShell Installer (`install.ps1`)

Windows 사용자를 위한 PowerShell 스크립트.

### 동작

```
1. 아키텍처 확인 (x86_64 고정 — ARM Windows 미지원)
2. GitHub Release에서 belt-x86_64-pc-windows-msvc.zip 다운로드
3. $INSTALL_DIR (기본 $HOME\.belt\bin) 에 압축 해제
4. User PATH에 설치 경로 추가 (레지스트리)
5. 현재 세션 PATH 갱신
```

### 환경 변수

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `BELT_INSTALL_DIR` | `$HOME\.belt\bin` | 설치 경로 |
| `BELT_VERSION` | latest | 특정 버전 지정 |

### PATH 설정

User 환경변수 레지스트리(`HKCU:\Environment\Path`)에 설치 경로를 추가한다. 관리자 권한 불필요.

### 의존성

- PowerShell 5.1+ (Windows 10 기본 탑재)
- `Invoke-WebRequest` (기본 cmdlet)
- `Expand-Archive` (기본 cmdlet)

---

## Release Workflow 연동

`release.yml`이 `v*` 태그 푸시 시 자동 실행:

```
1. 5개 타겟 병렬 빌드 (기존 구현)
2. GitHub Release 생성 + 바이너리 첨부 (기존 구현)
3. install.sh / install.ps1 은 릴리즈 바이너리를 참조 (신규)
```

installer 스크립트는 프로젝트 루트에 위치하며, `main` 브랜치의 raw URL로 접근한다:
- `https://raw.githubusercontent.com/kys0213/belt/main/install.sh`
- `https://raw.githubusercontent.com/kys0213/belt/main/install.ps1`

---

## 수용 기준

1. `curl -sSf https://.../install.sh | sh` 로 macOS/Linux에서 belt 설치 가능
2. `irm https://.../install.ps1 | iex` 로 Windows에서 belt 설치 가능
3. 설치된 belt 바이너리가 `belt --version` 으로 버전 출력
4. `BELT_VERSION=v0.1.0` 으로 특정 버전 설치 가능
5. `BELT_INSTALL_DIR` 으로 설치 경로 커스터마이징 가능
6. 네트워크 에러, 미지원 플랫폼 등에 명확한 에러 메시지

## 우선순위

| 순위 | 항목 | 근거 |
|------|------|------|
| P0 | install.sh + install.ps1 | 모든 사용자의 첫 진입점 |
