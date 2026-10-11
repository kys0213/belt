//! Fake-environment E2E tests: launch the built `belt` binary (daemon and
//! CLI) as subprocesses with no real GitHub or LLM calls.
//!
//! This binary has two roles, chosen by `argv[0]`:
//! - `gh` / `claude` (reached through symlinks in the sandbox `bin/`, first
//!   on the daemon's PATH): act as the fake CLI, state in `BELT_E2E_FAKE_DIR`.
//! - anything else: run the scenarios. A custom runner (`harness = false`)
//!   is needed because libtest owns `main`.
//!
//! Runner arguments mimic libtest: a positional substring filter,
//! `--ignored` (only ignored tests) and `--include-ignored`. Other flags
//! (e.g. `--nocapture`) are accepted and ignored. Tests run sequentially.

#[cfg(unix)]
mod fake_claude;
#[cfg(unix)]
mod fake_gh;
#[cfg(unix)]
mod harness;
#[cfg(unix)]
mod scenarios;

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
        ignore: Some("finding: belt stop sends SIGTERM but the daemon only drains on SIGINT"),
        run: scenarios::finding_belt_stop_shuts_daemon_down_gracefully,
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

#[cfg(unix)]
fn run_cases(args: Vec<String>) -> i32 {
    let only_ignored = args.iter().any(|a| a == "--ignored");
    let include_ignored = args.iter().any(|a| a == "--include-ignored");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();

    let selected: Vec<&Case> = CASES
        .iter()
        .filter(|c| filters.is_empty() || filters.iter().any(|f| c.name.contains(f.as_str())))
        .collect();
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
