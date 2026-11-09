//! Export laws without replacing user-owned text outside ELO markers.
use crate::bench::fixture::{Item, Kind};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Agents,
    Cursor,
    Copilot,
    Windsurf,
    #[value(name = "windsurf-legacy", alias = "windsurf-legado")]
    WindsurfLegado,
    Cline,
    Gemini,
    Todos,
}
const BEGIN: &str = "<!-- elo:begin -->";
const END: &str = "<!-- elo:end -->";

fn filename(id: &str) -> String {
    let slug: String = id
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Keep readable ids unchanged; transformed ids carry a digest to avoid collisions.
    if slug == id && !slug.is_empty() && slug.len() <= 100 {
        slug
    } else {
        let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
        format!(
            "{}-{}",
            slug.chars().take(100).collect::<String>(),
            &digest[..16]
        )
    }
}
fn globs(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .flat_map(|p| {
            if p.ends_with('*') {
                vec![p.clone(), format!("{p}/**")]
            } else {
                vec![p.clone()]
            }
        })
        .collect()
}
fn block(item: &Item) -> String {
    let mut text = format!("## {}\n\n{}\n", item.title, item.body);
    if !item.triggers.keywords.is_empty() {
        text.push_str(&format!(
            "\nKeywords: {}\n",
            item.triggers.keywords.join(", ")
        ));
    }
    if item.scope != "global" {
        text.push_str(&format!("\nScope: {}\n", item.scope));
    }
    text
}
fn merge(old: &str, body: &str) -> Result<String, String> {
    let replacement = format!("{BEGIN}\n{body}{END}");
    match (old.find(BEGIN), old.find(END)) {
        (None, None) => Ok(format!(
            "{old}{}{replacement}\n",
            if old.is_empty() || old.ends_with('\n') {
                ""
            } else {
                "\n"
            }
        )),
        (Some(a), Some(b))
            if a < b && old.matches(BEGIN).count() == 1 && old.matches(END).count() == 1 =>
        {
            Ok(format!(
                "{}{replacement}{}",
                &old[..a],
                &old[b + END.len()..]
            ))
        }
        _ => Err("incomplete or duplicate ELO markers; file preserved".into()),
    }
}
fn check_path(output: &Path, path: &Path) -> Result<(), String> {
    for part in path.ancestors().take_while(|p| p.starts_with(output)) {
        if fs::symlink_metadata(part).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("symbolic link preserved: {}", part.display()));
        }
    }
    Ok(())
}

// Write a sibling first: a failed write must not truncate the user's shared file.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_extension(format!("elo-export-{}-{stamp}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn run(
    root: &Path,
    output: &Path,
    format: Format,
    scope: Option<&str>,
    dry: bool,
) -> Result<String, String> {
    let store = crate::store::Store::open_no_migrate(root).map_err(|e| e.to_string())?;
    let mut items = crate::live::load_items(&store).map_err(|e| e.to_string())?;
    let protected: BTreeSet<String> = items
        .iter()
        .filter(|i| {
            i.kind == Kind::Law
                && crate::hook::deliverable(i, chrono::Utc::now())
                && scope.is_some_and(|s| i.scope != s)
        })
        .map(|i| filename(&i.id))
        .collect();
    items.retain(|i| {
        i.kind == Kind::Law
            && crate::hook::deliverable(i, chrono::Utc::now())
            && scope.is_none_or(|s| i.scope == s)
    });
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let mut names = BTreeSet::new();
    for item in &items {
        if !names.insert(filename(&item.id)) {
            return Err("filename collision; export cancelled".into());
        }
        if block(item).contains(BEGIN) || block(item).contains(END) {
            return Err("law contains a reserved ELO marker; export cancelled".into());
        }
    }
    let common = items
        .iter()
        .filter(|i| i.triggers.always || !i.triggers.keywords.is_empty())
        .map(block)
        .collect::<Vec<_>>()
        .join("\n");
    let always = items
        .iter()
        .filter(|i| i.triggers.always)
        .map(block)
        .collect::<Vec<_>>()
        .join("\n");
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut shared = BTreeSet::new();
    let mut directories: Vec<(&str, &str)> = Vec::new();
    let selected = |f| format == f || format == Format::Todos;
    // Keyword laws have no portable native trigger; retain their words in AGENTS.md.
    shared.insert(PathBuf::from("AGENTS.md"));
    files.insert("AGENTS.md".into(), common.clone());
    if selected(Format::Gemini) {
        shared.insert("GEMINI.md".into());
        files.insert("GEMINI.md".into(), common);
    }
    if selected(Format::Copilot) {
        shared.insert(".github/copilot-instructions.md".into());
        files.insert(".github/copilot-instructions.md".into(), always.clone());
        directories.push((".github/instructions", ".instructions.md"));
    }
    if selected(Format::Cursor) {
        shared.insert(".cursor/rules/elo.mdc".into());
        files.insert(
            ".cursor/rules/elo.mdc".into(),
            format!("---\ndescription: ELO\nalwaysApply: true\n---\n\n{always}"),
        );
        directories.push((".cursor/rules", ".mdc"));
    }
    let windsurf_dir = if format == Format::WindsurfLegado {
        ".windsurf/rules"
    } else {
        ".devin/rules"
    };
    let windsurf = selected(Format::Windsurf) || format == Format::WindsurfLegado;
    if windsurf {
        directories.push((windsurf_dir, ".md"));
    }
    let cline_file = output.join(".clinerules").is_file();
    if selected(Format::Cline) {
        if cline_file {
            shared.insert(".clinerules".into());
            files.insert(
                ".clinerules".into(),
                items.iter().map(block).collect::<Vec<_>>().join("\n"),
            );
        } else {
            directories.push((".clinerules", ".md"));
        }
    }
    for item in &items {
        let id = filename(&item.id);
        let patterns = globs(&item.triggers.paths);
        let list = serde_json::to_string(&patterns).unwrap();
        let joined = serde_json::to_string(&patterns.join(",")).unwrap();
        let description = serde_json::to_string(&item.title).unwrap();
        let body = block(item);
        if !patterns.is_empty() {
            if selected(Format::Cursor) {
                files.insert(format!(".cursor/rules/elo-{id}.mdc").into(),format!("---\ndescription: {description}\nglobs: {joined}\nalwaysApply: false\n---\n\n{body}"));
            }
            if selected(Format::Copilot) {
                files.insert(
                    format!(".github/instructions/elo-{id}.instructions.md").into(),
                    format!("---\napplyTo: {joined}\n---\n\n{body}"),
                );
            }
        }
        if item.triggers.always || !patterns.is_empty() {
            if windsurf {
                files.insert(
                    format!("{windsurf_dir}/elo-{id}.md").into(),
                    format!(
                        "---\ntrigger: {}\nglobs: {list}\n---\n\n{body}",
                        if item.triggers.always {
                            "always_on"
                        } else {
                            "glob"
                        }
                    ),
                );
            }
            if selected(Format::Cline) && !cline_file {
                files.insert(
                    format!(".clinerules/elo-{id}.md").into(),
                    format!(
                        "---\npaths: {}\n---\n\n{body}",
                        if item.triggers.always {
                            "[\"**\"]"
                        } else {
                            &list
                        }
                    ),
                );
            }
        }
    }
    // Preflight every write before changing anything, including marker validation.
    let mut writes = BTreeMap::new();
    for (relative, body) in &files {
        let path = output.join(relative);
        check_path(output, &path)?;
        let old = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.to_string()),
        };
        let text = if shared.contains(relative) {
            if relative == Path::new(".cursor/rules/elo.mdc") {
                let (frontmatter, rules) = body.split_once("\n\n").unwrap();
                let base = if old.is_empty() {
                    format!("{frontmatter}\n\n")
                } else {
                    old.clone()
                };
                merge(&base, rules)?
            } else {
                merge(&old, body)?
            }
        } else {
            body.clone()
        };
        if text != old {
            writes.insert(path, text);
        }
    }
    let mut deletes = Vec::new();
    for (dir, extension) in directories {
        let path = output.join(dir);
        check_path(output, &path)?;
        if !path.exists() {
            continue;
        }
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name
                .strip_prefix("elo-")
                .and_then(|n| n.strip_suffix(extension))
                .is_some_and(|id| protected.contains(id))
            {
                continue;
            }
            if name.starts_with("elo-")
                && name.ends_with(extension)
                && !files.contains_key(&Path::new(dir).join(name.as_ref()))
            {
                check_path(output, &entry.path())?;
                if entry.file_type().map_err(|e| e.to_string())?.is_file() {
                    deletes.push(entry.path());
                }
            }
        }
    }
    deletes.sort();
    let mut report = String::new();
    for (path, text) in writes {
        report.push_str(&format!(
            "{} {}\n",
            if dry { "would write" } else { "wrote" },
            path.display()
        ));
        if !dry {
            fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            atomic_write(&path, &text).map_err(|e| e.to_string())?;
        }
    }
    for path in deletes {
        report.push_str(&format!(
            "{} {}\n",
            if dry { "would remove" } else { "removed" },
            path.display()
        ));
        if !dry {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(report)
}
