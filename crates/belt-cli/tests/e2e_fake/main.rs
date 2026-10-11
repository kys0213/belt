//! Fake-environment E2E tests: launch the built `belt` binary (daemon and
//! CLI) as subprocesses with no real GitHub or LLM calls.
//!
//! This binary has two roles, chosen by `argv[0]`:
//! - `gh` / `claude` (reached through symlinks in the sandbox `bin/`, first
//!   on the daemon's PATH): act as the fake CLI, state in `BELT_E2E_FAKE_DIR`.
//! - anything else: run the scenarios. A custom runner (`harness = false`)
//!   is needed because libtest owns `main`.
//!
//! Runner arguments mimic libtest: positional substring filters, `--skip X`,
//! `--exact`, `--list`, `--ignored` (only ignored tests) and
//! `--include-ignored`. Output/scheduling flags (`--nocapture`,
//! `--test-threads N`, ...) are accepted and ignored; any other flag exits 2
//! instead of being taken as a filter. Tests run sequentially.

#[cfg(unix)]
mod fake_claude;
#[cfg(unix)]
mod fake_gh;
#[cfg(unix)]
mod harness;
#[cfg(unix)]
mod scenarios;
#[cfg(unix)]
mod scenarios_cancel;
#[cfg(unix)]
mod scenarios_fixes;
#[cfg(unix)]
mod scenarios_hitl;
#[cfg(unix)]
mod scenarios_lineage;
#[cfg(unix)]
mod scenarios_migration;
#[cfg(unix)]
mod support;

/// One registered scenario. `ignore` carries the reason, like
/// `#[ignore = "..."]`; E2E findings use `"finding: ..."`.
#[cfg(unix)]
struct Case {
    name: &'static str,
    ignore: Option<&'static str>,
    run: fn(),
}

#[cfg(unix)]
const CASES: &[Case] = &[
    Case {
        name: "smoke_labeled_issue_reaches_done_and_worktree_is_cleaned",
        ignore: None,
        run: scenarios::smoke_labeled_issue_reaches_done_and_worktree_is_cleaned,
    },
    Case {
        name: "finding_belt_stop_shuts_daemon_down_gracefully",
        ignore: None,
        run: scenarios::finding_belt_stop_shuts_daemon_down_gracefully,
    },
    Case {
        name: "lineage_handler_failure_retry_derives_item_that_inherits_worktree",
        ignore: None,
        run: scenarios_lineage::handler_failure_retry_derives_item_that_inherits_worktree,
    },
    Case {
        name: "lineage_failed_item_blocks_recollection_across_scans_and_restart",
        ignore: None,
        run: scenarios_lineage::failed_item_blocks_recollection_across_scans_and_restart,
    },
    Case {
        name: "finding_daemon_survives_a_registered_workspace",
        ignore: None,
        run: scenarios_lineage::finding_daemon_survives_a_registered_workspace,
    },
    Case {
        name: "hitl_resolved_on_github_then_late_cli_answer_is_already_handled",
        ignore: None,
        run: scenarios_hitl::hitl_resolved_on_github_then_late_cli_answer_is_already_handled,
    },
    Case {
        name: "hitl_needs_human_label_added_on_open_and_removed_on_resolve",
        ignore: None,
        run: scenarios_hitl::hitl_needs_human_label_added_on_open_and_removed_on_resolve,
    },
    Case {
        name: "hitl_resolution_is_logged_as_hitl_resolved_transition",
        ignore: None,
        run: scenarios_hitl::hitl_resolution_is_logged_as_hitl_resolved_transition,
    },
    Case {
        name: "cancel_skip_running_item_through_daemon_kills_handler_group",
        ignore: None,
        run: scenarios_cancel::skip_running_item_cancels_through_daemon_and_kills_handler_group,
    },
    Case {
        name: "cancel_skip_running_item_without_daemon_cancels_directly",
        ignore: None,
        run: scenarios_cancel::skip_running_item_without_daemon_cancels_directly,
    },
    Case {
        name: "cancel_direct_cancel_records_skipped_attempt",
        ignore: None,
        run: scenarios_cancel::direct_cancel_records_skipped_attempt,
    },
    Case {
        name: "cancel_direct_cancel_of_derived_item_cleans_inherited_worktree",
        ignore: None,
        run: scenarios_cancel::direct_cancel_of_derived_item_cleans_inherited_worktree,
    },
    Case {
        name: "migration_v1_database_is_backed_up_and_migrated_on_daemon_start",
        ignore: None,
        run: scenarios_migration::v1_database_is_backed_up_and_migrated_on_daemon_start,
    },
    Case {
        name: "migration_v1_database_with_unknown_phase_is_refused_without_backup",
        ignore: None,
        run: scenarios_migration::v1_database_with_unknown_phase_is_refused_without_backup,
    },
    Case {
        name: "hitl_unauthorized_answer_is_rejected_once_and_work_id_answer_follows_post_processing",
        ignore: None,
        run: scenarios_hitl::hitl_unauthorized_answer_is_rejected_once_and_work_id_answer_follows_post_processing,
    },
    Case {
        name: "cancel_skip_after_handler_finished_is_too_late",
        ignore: None,
        run: scenarios_cancel::skip_after_handler_finished_is_too_late,
    },
    Case {
        name: "cancel_skip_of_queued_item_is_applied",
        ignore: None,
        run: scenarios_cancel::skip_of_queued_item_is_applied,
    },
    Case {
        name: "cancel_skip_with_unresponsive_daemon_cancels_directly_and_late_result_conflicts",
        ignore: None,
        run: scenarios_cancel::skip_with_unresponsive_daemon_cancels_directly_and_late_result_conflicts,
    },
    Case {
        name: "lineage_second_failure_retries_with_comment_and_runs_on_fail",
        ignore: None,
        run: scenarios_lineage::second_failure_retries_with_comment_and_runs_on_fail,
    },
    Case {
        name: "migration_v1_backup_with_taken_name_goes_to_next_free_name",
        ignore: None,
        run: scenarios_migration::v1_backup_with_taken_name_goes_to_next_free_name,
    },
    Case {
        name: "fixes_sigint_during_a_tick_shuts_the_daemon_down",
        ignore: None,
        run: scenarios_fixes::sigint_during_a_tick_shuts_the_daemon_down,
    },
    Case {
        name: "fixes_sigterm_drains_a_running_handler",
        ignore: None,
        run: scenarios_fixes::sigterm_drains_a_running_handler,
    },
    Case {
        name: "fixes_skip_of_pending_derived_item_cleans_inherited_worktree",
        ignore: None,
        run: scenarios_fixes::skip_of_pending_derived_item_cleans_inherited_worktree,
    },
    Case {
        name: "fixes_skip_of_failed_derived_item_cleans_inherited_worktree",
        ignore: None,
        run: scenarios_fixes::skip_of_failed_derived_item_cleans_inherited_worktree,
    },
    Case {
        name: "fixes_done_of_completed_derived_item_uses_and_cleans_inherited_worktree",
        ignore: None,
        run: scenarios_fixes::done_of_completed_derived_item_uses_and_cleans_inherited_worktree,
    },
];

#[cfg(unix)]
fn main() {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let program = std::path::Path::new(&argv0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if program == "gh" || program == "claude" {
        let Some(dir) = std::env::var_os(harness::FAKE_DIR_ENV) else {
            eprintln!("fake {program}: {} is not set", harness::FAKE_DIR_ENV);
            std::process::exit(97);
        };
        let dir = std::path::PathBuf::from(dir);
        let rest: Vec<String> = args.collect();
        let code = if program == "gh" {
            fake_gh::main(&dir, rest)
        } else {
            fake_claude::main(&dir, rest)
        };
        std::process::exit(code);
    }
    std::process::exit(run_cases(args.collect()));
}

/// Runner options, parsed like libtest's.
#[cfg(unix)]
#[derive(Default)]
struct RunnerArgs {
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
    only_ignored: bool,
    include_ignored: bool,
    list: bool,
}

#[cfg(unix)]
impl RunnerArgs {
    /// Flags without a value that only matter to libtest's own output.
    const NO_OP_FLAGS: &[&str] = &["--nocapture", "--show-output", "--quiet", "-q"];
    /// Flags with a value that only matter to libtest's own scheduling/output.
    const NO_OP_VALUE_FLAGS: &[&str] = &["--test-threads", "--color", "--format"];

    /// Unknown flags are rejected rather than mistaken for a filter.
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) if arg.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let mut value = |flag: &str| {
                inline
                    .clone()
                    .or_else(|| iter.next())
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match name.as_str() {
                "--ignored" => parsed.only_ignored = true,
                "--include-ignored" => parsed.include_ignored = true,
                "--exact" => parsed.exact = true,
                "--list" => parsed.list = true,
                "--skip" => parsed.skips.push(value("--skip")?),
                f if Self::NO_OP_FLAGS.contains(&f) => {}
                f if Self::NO_OP_VALUE_FLAGS.contains(&f) => {
                    value(f)?;
                }
                f if f.starts_with('-') => return Err(format!("unsupported flag: {arg}")),
                _ => parsed.filters.push(arg),
            }
        }
        Ok(parsed)
    }

    fn matches(&self, pattern: &str, name: &str) -> bool {
        if self.exact {
            name == pattern
        } else {
            name.contains(pattern)
        }
    }

    fn selects(&self, name: &str) -> bool {
        (self.filters.is_empty() || self.filters.iter().any(|f| self.matches(f, name)))
            && !self.skips.iter().any(|s| self.matches(s, name))
    }
}

#[cfg(unix)]
fn run_cases(args: Vec<String>) -> i32 {
    let args = match RunnerArgs::parse(args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("e2e_fake: {e}");
            return 2;
        }
    };
    let only_ignored = args.only_ignored;
    let include_ignored = args.include_ignored;

    let selected: Vec<&Case> = CASES.iter().filter(|c| args.selects(c.name)).collect();
    if args.list {
        for case in &selected {
            println!("{}: test", case.name);
        }
        return 0;
    }
    println!("\nrunning {} tests", selected.len());

    let (mut passed, mut failed, mut ignored) = (0, Vec::new(), 0);
    for case in selected {
        let skip = match case.ignore {
            Some(_) => !(only_ignored || include_ignored),
            None => only_ignored,
        };
        if skip {
            match case.ignore {
                Some(reason) => println!("test {} ... ignored, {reason}", case.name),
                None => println!("test {} ... ignored", case.name),
            }
            ignored += 1;
            continue;
        }
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(case.run);
        let secs = start.elapsed().as_secs_f32();
        match result {
            Ok(()) => {
                println!("test {} ... ok ({secs:.1}s)", case.name);
                passed += 1;
            }
            Err(_) => {
                println!("test {} ... FAILED ({secs:.1}s)", case.name);
                failed.push(case.name);
            }
        }
    }

    let status = if failed.is_empty() { "ok" } else { "FAILED" };
    if !failed.is_empty() {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
    }
    println!(
        "\ntest result: {status}. {passed} passed; {} failed; {ignored} ignored\n",
        failed.len()
    );
    if failed.is_empty() { 0 } else { 101 }
}

#[cfg(not(unix))]
fn main() {
    // The fakes rely on symlinks and POSIX signals; CI runs Linux and macOS.
    println!("\nrunning 0 tests (fake-environment E2E is unix-only)\n");
}
