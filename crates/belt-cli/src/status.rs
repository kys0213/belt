//! Status display for `belt status`.
//!
//! Supports `text`, `json`, and `rich` output formats.
//! The `rich` format includes runtime statistics alongside system status.

use belt_infra::db::{Database, RuntimeStats};
use serde::Serialize;

use crate::dashboard;

/// Summary payload for JSON output.
#[derive(Serialize)]
struct StatusOutput {
    status: &'static str,
    runtime_stats: Option<RuntimeStats>,
}

/// Display system status in the requested format.
///
/// # Errors
/// Returns an error if the database cannot be opened or queried.
pub fn show_status(format: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    let stats = db.get_runtime_stats().ok();

    match format {
        "json" => {
            let output = StatusOutput {
                status: "ok",
                runtime_stats: stats,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        "rich" => {
            println!("Belt System Status: OK");
            println!();
            if let Some(ref s) = stats {
                dashboard::render_runtime_panel(s);
            } else {
                println!("  Runtime stats: unavailable");
            }
        }
        _ => {
            // Default text format.
            println!("Belt System Status: OK");
            if let Some(ref s) = stats {
                println!(
                    "  Tokens (24h): {} total, {} executions",
                    s.total_tokens, s.executions
                );
            }
        }
    }

    Ok(())
}

/// Open the Belt database from the default location (`~/.belt/belt.db`).
fn open_db() -> anyhow::Result<Database> {
    let belt_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
        .join(".belt");
    let db_path = belt_home.join("belt.db");
    let db = Database::open(
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid database path"))?,
    )?;
    Ok(db)
}
