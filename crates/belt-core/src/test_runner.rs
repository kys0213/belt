//! TestRunner trait and result types for spec verification.
//!
//! Defines the interface for executing test commands associated with a spec.
//! Concrete implementations live in `belt-infra`.

use std::path::Path;

use async_trait::async_trait;

use crate::error::BeltError;

/// Result of executing a single test command.
#[derive(Debug, Clone)]
pub struct TestCommandResult {
    /// The command that was executed.
    pub command: String,
    /// Whether the command exited successfully (exit code 0).
    pub success: bool,
    /// Combined stdout/stderr output (may be truncated).
    pub output: String,
}

/// Aggregate result of running all test commands for a spec.
#[derive(Debug, Clone)]
pub struct TestRunResult {
    /// Individual command results.
    pub results: Vec<TestCommandResult>,
    /// `true` only when every command succeeded.
    pub all_passed: bool,
}

/// Trait for executing spec verification test commands.
///
/// Implementations receive a list of shell commands and a working directory,
/// then execute them sequentially and report results. The `fail_fast` flag
/// controls whether execution stops at the first failure.
#[async_trait]
pub trait TestRunner: Send + Sync {
    /// Run the given test commands in `working_dir`.
    ///
    /// When `fail_fast` is `true`, execution stops at the first failing command.
    /// Individual command failures are captured in [`TestRunResult`] rather than
    /// returned as errors. Errors are reserved for cases where a command cannot
    /// be spawned at all.
    async fn run(
        &self,
        commands: &[&str],
        working_dir: &Path,
        fail_fast: bool,
    ) -> Result<TestRunResult, BeltError>;
}
