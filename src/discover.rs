//! Zero-config project discovery (SPEC §2.1): projects come from package manifests,
//! never from a hand-written list. The workspace root itself is not a project when
//! nested projects exist; below it, the outermost manifest wins and nested ones are reported.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "pom.xml",
    "deno.json",
    "deno.jsonc",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
];
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "out",
    "coverage",
];
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub name: String,
    /// Workspace-relative root with a trailing `/`.
    pub root: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub projects: Vec<Discovered>,
    /// Manifests found inside an already discovered project (reported, not projects).
    pub nested: Vec<String>,
    /// True when the root has a manifest but no nested project: a single-project repo.
    pub single_root: bool,
    /// Git worktrees (a `.git` *file* pointing at another repository): copies of a
    /// project, not projects. `(worktree root, main repository folder)`.
    pub worktrees: Vec<(String, PathBuf)>,
}

/// If `dir` is a git worktree, the folder of its main repository (absolute or
/// resolved against `dir`). Submodules (`.git/modules/...`) are not worktrees.
fn worktree_of(dir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dir.join(".git")).ok()?;
    let raw = text.strip_prefix("gitdir:")?.trim();
    if !raw.replace('\\', "/").contains(".git/worktrees/") {
        return None;
    }
    let gitdir = Path::new(raw);
    let resolved = if gitdir.is_relative() {
        dir.join(gitdir)
    } else {
        gitdir.to_path_buf()
    };
    let dot_git = resolved
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".git"))?;
    let main = dot_git.parent()?;
    Some(fs::canonicalize(main).unwrap_or_else(|_| main.to_path_buf()))
}

fn has_manifest(dir: &Path) -> bool {
    MANIFESTS.iter().any(|m| dir.join(m).is_file())
        || fs::read_dir(dir).is_ok_and(|rd| {
            rd.filter_map(Result::ok).any(|e| {
                e.file_type().is_ok_and(|t| t.is_file())
                    && e.file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .ends_with(".csproj")
            })
        })
}

fn skip(name: &str) -> bool {
    name.starts_with('.') || SKIP.contains(&name)
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    inside: bool,
    out: &mut Discovery,
    found: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !skip(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for sub in dirs {
        // Never leave the workspace (a path that is not under `root` is skipped).
        let Ok(rel) = sub.strip_prefix(root).map(Path::to_path_buf) else {
            continue;
        };
        let rel_text = rel.to_string_lossy().replace('\\', "/");
        if let Some(main) = worktree_of(&sub) {
            out.worktrees.push((format!("{rel_text}/"), main));
            continue;
        }
        let is_project = has_manifest(&sub);
        if is_project && inside {
            out.nested.push(format!("{rel_text}/"));
        } else if is_project {
            found.push(rel);
        }
        if depth + 1 < MAX_DEPTH {
            walk(root, &sub, depth + 1, inside || is_project, out, found);
        }
    }
}

/// Discovers the projects under `root`.
pub fn discover(root: &Path) -> Discovery {
    let mut out = Discovery::default();
    let mut found = Vec::new();
    walk(root, root, 0, false, &mut out, &mut found);
    if found.is_empty() {
        out.single_root = has_manifest(root);
        return out;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for rel in &found {
        let base = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        *seen.entry(base).or_default() += 1;
    }
    for rel in found {
        let text = rel.to_string_lossy().replace('\\', "/");
        let base = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Repeated folder names get their full path as the name (`services-api`).
        let name = if seen.get(&base).copied().unwrap_or(0) > 1 {
            text.replace('/', "-")
        } else {
            base
        };
        let mut name = sanitize(&name);
        let stem = name.clone();
        let mut n = 2;
        while out.projects.iter().any(|p| p.name == name) {
            name = format!("{stem}-{n}");
            n += 1;
        }
        out.projects.push(Discovered {
            name,
            root: format!("{text}/"),
        });
    }
    out
}

/// Project names keep only `[A-Za-z0-9._-]`; anything else becomes `-`.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "project".into()
    } else {
        s
    }
}

/// A TOML basic string: escapes `\\`, `"` and control characters as `\\uXXXX`.
fn toml_str(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `[[project]]` blocks for `elo.toml`.
pub fn to_toml(projects: &[Discovered]) -> String {
    let mut s = String::new();
    for p in projects {
        s.push_str(&format!(
            "\n[[project]]\nname = {}\nroot = {}\naliases = [{}]\n",
            toml_str(&p.name),
            toml_str(&p.root),
            toml_str(&p.name)
        ));
    }
    s
}
