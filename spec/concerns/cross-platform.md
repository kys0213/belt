# Cross-Platform Compatibility

Belt는 Linux, macOS, Windows에서 동작해야 한다. 릴리스 바이너리는 5개 타겟으로 빌드된다.

## 빌드 타겟

| Target | OS | Arch | Archive |
|--------|-----|------|---------|
| x86_64-unknown-linux-gnu | Linux | x64 | tar.gz |
| aarch64-unknown-linux-gnu | Linux | ARM64 | tar.gz (cross) |
| x86_64-apple-darwin | macOS | x64 | tar.gz |
| aarch64-apple-darwin | macOS | ARM64 | tar.gz |
| x86_64-pc-windows-msvc | Windows | x64 | zip |

## 현재 플랫폼 의존성 목록

### 1. Unix Signal (SIGUSR1)

**영향 범위**: daemon 실행 루프, cron trigger CLI

| 파일 | 용도 | Windows 대안 |
|------|------|-------------|
| `daemon.rs` `run_select_loop` | SIGUSR1로 cron 동기화 트리거 | 없음 (tick 폴링으로 fallback) |
| `daemon.rs` `handle_cron_trigger_signal` | SIGUSR1 수신 핸들러 | `#[cfg(unix)]`로 게이트 |
| `main.rs` `signal_daemon` | PID 파일 → `kill -USR1` 전송 | 미구현 (`anyhow::bail!`) |

**대안 설계**: Windows에서는 named pipe, TCP localhost, 또는 파일 기반 polling으로 daemon ↔ CLI 통신 구현.

### 2. Shell 실행 (`sh -c`, `bash -c`)

**영향 범위**: handler script 실행, cron script, test runner, hook.on_done()/hook.on_fail()

| 파일 | 셸 | Windows 호환 |
|------|-----|-------------|
| `executor.rs` | `bash -c` | ❌ bash 미설치 시 실패 |
| `cron.rs` (ScriptJob) | `sh -c` | ❌ sh 미존재 |
| `test_runner.rs` | `sh -c` | ❌ sh 미존재 |
| `main.rs` (cron run) | `sh -c` | ❌ sh 미존재 |

**대안 설계**: Windows에서는 `cmd.exe /C` 또는 `powershell -Command`로 분기. 또는 workspace yaml에 `shell: bash|cmd|pwsh` 설정 추가.

```rust
#[cfg(unix)]
fn shell_command(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd
}

#[cfg(windows)]
fn shell_command(script: &str) -> Command {
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/C").arg(script);
    cmd
}
```

### 3. 외부 CLI 의존성

| CLI | 용도 | 설치 요구 | Windows 호환 |
|-----|------|----------|-------------|
| `gh` | GitHub 이슈/PR 조회, 생성 | 필수 | ✅ (winget으로 설치 가능) |
| `claude` | LLM agent 호출 | handler prompt 실행 시 | ✅ (npm으로 설치 가능) |
| `git` | worktree 관리 | 필수 | ✅ |

외부 CLI는 모두 Windows 지원됨. 문제 없음.

### 4. 파일 경로

| 패턴 | 현재 | Windows 이슈 |
|------|------|-------------|
| `~/.belt/` | `dirs::home_dir().join(".belt")` | ✅ (`C:\Users\<user>\.belt`) |
| PID 파일 | `belt_home.join("belt.pid")` | ✅ |
| Worktree 경로 | `PathBuf` 사용 | ✅ (path separator 자동 변환) |
| `/` 하드코딩 | 일부 `format!` 내 | ⚠️ 점검 필요 |

**규칙**: 경로 조합은 반드시 `PathBuf::join()` 또는 `Path::join()`을 사용. 문자열 결합(`format!("{}/{}",...)`)으로 경로를 만들지 않는다.

### 5. Evaluator 테스트

| 파일 | 이슈 |
|------|------|
| `evaluator.rs` 테스트 | `#[cfg(unix)]`로 대부분 게이트됨 — Windows 테스트 커버리지 0% |

**대안**: 핵심 로직(prompt 생성, 결과 파싱)을 subprocess 호출과 분리하여 플랫폼 무관 테스트 가능하게 리팩터링.

## 추상화 설계 (OCP)

플랫폼 의존 코드를 trait으로 추상화하여, 새 OS 지원 시 기존 코드를 수정하지 않고 구현체만 추가한다.

### trait 정의 (`belt-core`)

```rust
// core/src/platform.rs

/// 플랫폼별 셸 스크립트 실행을 추상화한다.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// 셸 스크립트를 실행하고 결과를 반환한다.
    async fn execute(
        &self,
        script: &str,
        working_dir: &Path,
        env_vars: &HashMap<String, String>,
    ) -> Result<ShellOutput>;
}

pub struct ShellOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 플랫폼별 프로세스 간 통신을 추상화한다.
/// Daemon ↔ CLI 간 cron trigger 등에 사용.
#[async_trait]
pub trait DaemonNotifier: Send + Sync {
    /// Daemon에게 cron 동기화를 요청한다.
    async fn notify(&self, pid: u32) -> Result<()>;
}
```

### 구현체 (`belt-infra`)

```
infra/src/platform/
  mod.rs           // pub use + platform_default() 팩토리
  unix_shell.rs    // sh -c 기반 ShellExecutor
  unix_signal.rs   // SIGUSR1 기반 DaemonNotifier
  windows_shell.rs // cmd.exe /C 기반 ShellExecutor (향후)
  windows_ipc.rs   // named pipe 기반 DaemonNotifier (향후)
```

```rust
// infra/src/platform/mod.rs

mod unix_shell;
mod unix_signal;

/// 현재 플랫폼에 맞는 ShellExecutor를 반환한다.
pub fn default_shell() -> Box<dyn ShellExecutor> {
    #[cfg(unix)]
    { Box::new(unix_shell::UnixShell) }
    #[cfg(windows)]
    { Box::new(windows_shell::WindowsShell) }
}

/// 현재 플랫폼에 맞는 DaemonNotifier를 반환한다.
pub fn default_notifier() -> Box<dyn DaemonNotifier> {
    #[cfg(unix)]
    { Box::new(unix_signal::UnixSignalNotifier) }
    #[cfg(windows)]
    { Box::new(windows_ipc::WindowsIpcNotifier) }
}
```

### 소비자 변경

| 현재 | 변경 후 |
|------|---------|
| `executor.rs`: `Command::new("bash").arg("-c")` | `shell.execute(script, dir, vars)` |
| `cron.rs` ScriptJob: `Command::new("sh").arg("-c")` | `shell.execute(script, dir, vars)` |
| `test_runner.rs`: `Command::new("sh").arg("-c")` | `shell.execute(cmd, dir, vars)` |
| `main.rs` cron run: `Command::new("sh").arg("-c")` | `shell.execute(script, dir, vars)` |
| `main.rs` signal_daemon: `kill(pid, SIGUSR1)` | `notifier.notify(pid)` |
| `daemon.rs` run_select_loop: `signal::unix::signal(USR1)` | 플랫폼별 `#[cfg]` 유지 (이벤트 루프는 trait 추상화 어려움) |

### 주입 경로

```
Daemon::new(config, sources, registry, worktree_mgr, shell, max_concurrent)
                                                      ^^^^^
ActionExecutor::new(registry, shell)
                              ^^^^^
```

`Daemon`과 `ActionExecutor`가 생성 시점에 `Box<dyn ShellExecutor>`를 받는다.
테스트에서는 `MockShell`을 주입하여 subprocess 없이 검증 가능.

## 수용 기준

1. `cargo build --target <target>` 가 5개 타겟 모두에서 경고 없이 성공
2. `cargo test --workspace`가 Linux, macOS, Windows CI에서 모두 통과
3. Unix 전용 코드는 `#[cfg(unix)]`로 명시적 게이트
4. Windows에서 미구현 기능은 `#[cfg(not(unix))]` 블록에 명확한 에러 메시지 또는 fallback
5. Shell script 실행 시 플랫폼별 셸 자동 선택 (`sh`/`cmd.exe`)
6. 파일 경로에 `/` 하드코딩 없음 — `Path::join()` 사용 강제

## 우선순위

| 순위 | 항목 | 근거 |
|------|------|------|
| P0 | CI 통과 (warnings as errors) | 모든 플랫폼에서 빌드/테스트 통과 보장 |
| P1 | Shell 실행 분기 | handler script가 핵심 기능 |
| P2 | Daemon IPC (signal 대안) | Windows에서 cron trigger 지원 |
| P3 | Evaluator 테스트 크로스 플랫폼 | 테스트 커버리지 확보 |
