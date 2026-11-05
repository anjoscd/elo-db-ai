//! Parsing and validation of `elo.toml`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// Name of the configuration file at the workspace root.
pub const CONFIG_FILE: &str = "elo.toml";

pub const DEFAULT_BUDGET_BYTES: u64 = 9_000;
pub const DEFAULT_MIN_SCORE: f64 = 0.35;
pub const DEFAULT_PROJECT_QUOTA: f64 = 0.50;
pub const DEFAULT_LINK_QUOTA: f64 = 0.25;
pub const DEFAULT_GLOBAL_QUOTA: f64 = 0.25;

/// Quota sums are compared against 1.0 with this tolerance.
const QUOTA_TOLERANCE: f64 = 1e-6;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("workspace.budget_bytes must be greater than zero")]
    BudgetEmpty,
    #[error("workspace.min_score must be between 0 and 1, got {0}")]
    MinScoreRange(f64),
    #[error("quota `{name}` must be between 0 and 1, got {value}")]
    QuotaRange { name: &'static str, value: f64 },
    #[error("quotas must add up to 1.0, got {0}")]
    QuotaSum(f64),
    #[error("project name must not be empty")]
    ProjectNameEmpty,
    #[error("project `{0}` has an empty root")]
    ProjectRootEmpty(String),
    #[error("project root must be relative and inside the workspace: {0}")]
    InvalidRoot(PathBuf),
    #[error("link `{link}` repeats project `{project}`")]
    DuplicateLinkProject { link: String, project: String },
    #[error("link `{link}` references unknown project `{project}`")]
    UnknownLinkProject { link: String, project: String },
    #[error("duplicate project name `{0}`")]
    DuplicateProject(String),
    #[error("link name must not be empty")]
    LinkNameEmpty,
    #[error("duplicate link name `{0}`")]
    DuplicateLink(String),
    #[error("link `{name}` needs at least 2 known projects, found {found}")]
    LinkTooSmall { name: String, found: usize },
}

/// Parsed representation of `elo.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub workspace: Workspace,
    #[serde(rename = "project")]
    pub projects: Vec<Project>,
    #[serde(rename = "link")]
    pub links: Vec<Link>,
    pub quotas: Quotas,
    pub work: Work,
    /// `[[guard]]`: laws checked before each tool call (`elo hook pre-tool-use`).
    #[serde(rename = "guard")]
    pub guards: Vec<crate::guard::Guard>,
}

/// `[work]`: projects, tasks and handoffs (`.elo/work.db`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Work {
    /// Open items untouched for this many days become deferred; zero disables it.
    pub stale_days: u32,
}

impl Default for Work {
    fn default() -> Self {
        Self { stale_days: 10 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Workspace {
    pub budget_bytes: u64,
    pub min_score: f64,
    pub index_over_bytes: u64,
    /// Zero removes the cap on full notes/decisions (they still need 2+ keywords or a path).
    pub full_notes_max: usize,
    /// Per-event note/decision bytes; zero disables this ceiling.
    pub notes_budget_bytes: u64,
    /// Zero disables feedback demotion.
    pub demote_bad_min: u64,
    /// Maximum descendant projects activated by a parent cwd; zero disables it.
    pub parent_scope_max: usize,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            budget_bytes: DEFAULT_BUDGET_BYTES,
            min_score: DEFAULT_MIN_SCORE,
            index_over_bytes: 300,
            full_notes_max: 2,
            notes_budget_bytes: 1500,
            demote_bad_min: 3,
            parent_scope_max: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Link {
    pub name: String,
    pub projects: Vec<String>,
    pub keywords: Vec<String>,
    pub code_signals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Quotas {
    pub project: f64,
    pub link: f64,
    pub global: f64,
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            project: DEFAULT_PROJECT_QUOTA,
            link: DEFAULT_LINK_QUOTA,
            global: DEFAULT_GLOBAL_QUOTA,
        }
    }
}

impl Config {
    /// Loads `elo.toml` from `root`.
    pub fn load(root: &Path) -> Result<Self, ConfigError> {
        let path = root.join(CONFIG_FILE);
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse(&text, &path)
    }

    /// Parses and validates configuration text; `origin` only labels errors.
    pub fn parse(text: &str, origin: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Checks the rules that the TOML schema cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.workspace.budget_bytes == 0 {
            return Err(ConfigError::BudgetEmpty);
        }
        if !(0.0..=1.0).contains(&self.workspace.min_score) {
            return Err(ConfigError::MinScoreRange(self.workspace.min_score));
        }
        self.quotas.validate()?;
        self.validate_projects()?;
        self.validate_links()
    }

    fn validate_projects(&self) -> Result<(), ConfigError> {
        let mut names = BTreeSet::new();
        for project in &self.projects {
            if project.name.trim().is_empty() {
                return Err(ConfigError::ProjectNameEmpty);
            }
            if project.root.as_os_str().is_empty() {
                return Err(ConfigError::ProjectRootEmpty(project.name.clone()));
            }
            if !names.insert(project.name.as_str()) {
                return Err(ConfigError::DuplicateProject(project.name.clone()));
            }
        }
        self.validate_roots()
    }

    fn validate_roots(&self) -> Result<(), ConfigError> {
        for project in &self.projects {
            normalize(&project.root)?;
        }
        Ok(())
    }

    fn validate_links(&self) -> Result<(), ConfigError> {
        let known: BTreeSet<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
        let mut names = BTreeSet::new();
        for link in &self.links {
            if link.name.trim().is_empty() {
                return Err(ConfigError::LinkNameEmpty);
            }
            if !names.insert(link.name.as_str()) {
                return Err(ConfigError::DuplicateLink(link.name.clone()));
            }
            let mut distinct = BTreeSet::new();
            for project in &link.projects {
                if !distinct.insert(project) {
                    return Err(ConfigError::DuplicateLinkProject {
                        link: link.name.clone(),
                        project: project.clone(),
                    });
                }
                if !known.contains(project.as_str()) {
                    return Err(ConfigError::UnknownLinkProject {
                        link: link.name.clone(),
                        project: project.clone(),
                    });
                }
            }
            let found = distinct.len();
            if found < 2 {
                return Err(ConfigError::LinkTooSmall {
                    name: link.name.clone(),
                    found,
                });
            }
        }
        Ok(())
    }
}

impl Quotas {
    fn validate(&self) -> Result<(), ConfigError> {
        let values = [
            ("project", self.project),
            ("link", self.link),
            ("global", self.global),
        ];
        for (name, value) in values {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::QuotaRange { name, value });
            }
        }
        let sum = self.project + self.link + self.global;
        if (sum - 1.0).abs() > QUOTA_TOLERANCE {
            return Err(ConfigError::QuotaSum(sum));
        }
        Ok(())
    }
}

/// Resolves relative components without accessing the filesystem.
fn normalize(path: &Path) -> Result<Vec<OsString>, ConfigError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(ConfigError::InvalidRoot(path.to_path_buf()));
                }
            }
            _ => return Err(ConfigError::InvalidRoot(path.to_path_buf())),
        }
    }
    Ok(parts)
}
