//! Implementation of `elo doctor`.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::store::{Store, StoreError};

/// Runs every check, writing one line per check; returns `true` when all pass.
pub fn run(root: &Path, out: &mut dyn Write) -> std::io::Result<bool> {
    let mut healthy = true;

    match Config::load(root) {
        Ok(config) => {
            writeln!(
                out,
                "ok   config: {} project(s), {} link(s), budget {} bytes",
                config.projects.len(),
                config.links.len(),
                config.workspace.budget_bytes
            )?;
            if !config.guards.is_empty() {
                let problems = crate::guard::validate(&config.guards);
                let enforced = config.guards.iter().filter(|g| g.enforce).count();
                if problems.is_empty() {
                    writeln!(
                        out,
                        "ok   guards: {} ({} enforce, {} shadow)",
                        config.guards.len(),
                        enforced,
                        config.guards.len() - enforced
                    )?;
                } else {
                    healthy = false;
                    for problem in problems {
                        writeln!(out, "fail guards: {problem} (no guard is applied)")?;
                    }
                }
            }
        }
        Err(error) => {
            healthy = false;
            writeln!(out, "fail config: {error}")?;
        }
    }

    match Store::open(root).and_then(|store| summarize(&store)) {
        Ok(summary) => writeln!(out, "ok   store: {summary}")?,
        Err(error) => {
            healthy = false;
            writeln!(out, "fail store: {error}")?;
        }
    }

    warn_nested_roots(root, 0, out)?;
    Ok(healthy)
}

/// One line describing the database file and its schema version.
fn summarize(store: &Store) -> Result<String, StoreError> {
    store.check_schema()?;
    let version = store.schema_version()?;
    Ok(format!(
        "{} (schema version {})",
        store.path().display(),
        version
    ))
}

// Count project directories below the root, excluding the .elo directory itself.
fn warn_nested_roots(root: &Path, depth: usize, out: &mut dyn Write) -> std::io::Result<()> {
    if depth == 3 {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    let mut entries: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| !kind.is_symlink() && kind.is_dir())
                && !matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".elo"
                            | "node_modules"
                            | "target"
                            | ".git"
                            | ".venv"
                            | "venv"
                            | "build"
                            | "dist"
                            | ".next"
                            | ".cache"
                            | "vendor"
                            | ".dart_tool"
                            | "Pods"
                    )
                )
        })
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.join(".elo/elo.db").is_file() {
            writeln!(
                out,
                "warn nested ELO root: {} hides the workspace root laws for this subfolder; move its items to the workspace root and rename the nested .elo directory",
                path.join(".elo").display()
            )?;
        }
        warn_nested_roots(&path, depth + 1, out)?;
    }
    Ok(())
}
