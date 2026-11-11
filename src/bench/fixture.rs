//! Loading and structural validation of independently labelled fixtures.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize};

use crate::config::{Config, ConfigError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Law,
    Rule,
    Decision,
    Note,
    Pitfall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Superseded,
    Expired,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    User,
    Agent,
    Import,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    pub keywords: Vec<String>,
    pub paths: Vec<String>,
    pub always: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub kind: Kind,
    pub scope: String,
    pub title: String,
    pub body: String,
    pub triggers: Triggers,
    pub importance: f64,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
    pub status: Status,
    pub superseded_by: Option<String>,
    pub origin: Origin,
    #[serde(skip_deserializing)]
    pub feedback: crate::stats::Feedback,
    #[serde(skip_deserializing)]
    pub usage_window: crate::stats::UsageWindow,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub event_type: String,
    pub session_id: String,
    pub family: String,
    pub event_seq: u64,
    pub cwd: String,
    pub prompt: Option<String>,
    pub tool_paths: Option<Vec<String>>,
    pub now: String,
    pub allowed_scopes: Vec<String>,
    pub required: Vec<String>,
    pub forbidden: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Split {
    pub dev: Vec<String>,
    pub test: Vec<String>,
}

#[derive(Debug)]
pub struct Fixture {
    /// Folder the fixture was loaded from.
    pub dir: std::path::PathBuf,
    /// Folder whose files are read for code signals (`<dir>/workspace` for fixtures,
    /// the real workspace root when the selector runs live).
    pub code_root: std::path::PathBuf,
    pub config: Config,
    pub items: Vec<Item>,
    pub events: Vec<Event>,
    pub split: Split,
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}, line {line}, column {column}: {message}")]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    #[error("duplicate {entity} id `{id}`")]
    DuplicateId { entity: &'static str, id: String },
    #[error("{owner} references unknown item `{id}` in {field}")]
    UnknownItem {
        owner: String,
        field: &'static str,
        id: String,
    },
    #[error("event `{event}` requires and forbids `{id}`")]
    LabelOverlap { event: String, id: String },
    #[error("{owner} references unknown scope `{scope}`")]
    UnknownScope { owner: String, scope: String },
    #[error("split references unknown event `{0}`")]
    UnknownSplitEvent(String),
    #[error("split repeats event `{0}`")]
    DuplicateSplitEvent(String),
    #[error("split misses event `{0}`")]
    MissingSplitEvent(String),
    #[error("events `{first}` and `{second}` have duplicate normalized prompts")]
    DuplicatePrompt { first: String, second: String },
    #[error("{entity} `{id}` appears in both dev and test")]
    SplitLeak { entity: &'static str, id: String },
    #[error("event `{0}` has an empty scenario family")]
    EmptyFamily(String),
    #[error("law `{id}` has {bytes} bytes (maximum 400)")]
    LawTooLong { id: String, bytes: usize },
    #[error("{owner} has empty {field}")]
    EmptyId { owner: String, field: &'static str },
    #[error("event `{event}` repeats `{value}` in {field}")]
    DuplicateEntry {
        event: String,
        field: &'static str,
        value: String,
    },
    #[error("event `{0}` has an empty prompt")]
    EmptyPrompt(String),
    #[error("event `{0}` has neither prompt nor tool paths")]
    MissingToolPaths(String),
    #[error("event `{event}` requires `{id}` outside allowed scopes")]
    RequiredScope { event: String, id: String },
    #[error("event `{event}` requires inactive item `{id}`")]
    RequiredInactive { event: String, id: String },
    #[error("event `{event}` requires expired item `{id}`")]
    RequiredExpired { event: String, id: String },
    #[error("{owner} has invalid RFC3339 {field}: {value}")]
    InvalidTimestamp {
        owner: String,
        field: &'static str,
        value: String,
    },
    #[error("item `{0}` has inconsistent supersession status")]
    SupersessionStatus(String),
    #[error("item `{0}` has a successor of a different kind")]
    SupersessionKind(String),
    #[error("item `{0}` has a successor in a different scope")]
    SupersessionScope(String),
    #[error("supersession chain ends at inactive item `{0}`")]
    SupersessionInactiveEnd(String),
    #[error("law `{0}` must originate from user")]
    LawOrigin(String),
    #[error("law `{0}` cannot have an always trigger")]
    LawAlways(String),
    #[error("supersession cycle at `{0}`")]
    SupersessionCycle(String),
}

pub fn load_fixture(dir: impl AsRef<Path>) -> Result<Fixture, FixtureError> {
    let dir = dir.as_ref();
    let fixture = Fixture {
        dir: dir.to_path_buf(),
        code_root: dir.join("workspace"),
        config: Config::load(dir)?,
        items: jsonl(&dir.join("items.jsonl"))?,
        events: jsonl(&dir.join("events.jsonl"))?,
        split: parse(&read(&dir.join("split.json"))?, &dir.join("split.json"), 0)?,
    };
    fixture.validate()?;
    Ok(fixture)
}

fn read(path: &Path) -> Result<String, FixtureError> {
    fs::read_to_string(path).map_err(|source| FixtureError::Read {
        path: path.to_owned(),
        source,
    })
}
fn parse<T: DeserializeOwned>(text: &str, path: &Path, line: usize) -> Result<T, FixtureError> {
    serde_json::from_str(text).map_err(|error| FixtureError::Parse {
        path: path.to_owned(),
        line: if line == 0 { error.line() } else { line },
        column: error.column(),
        message: error.to_string(),
    })
}
fn jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, FixtureError> {
    read(path)?
        .lines()
        .enumerate()
        .map(|(index, line)| parse(line, path, index + 1))
        .collect()
}

impl Fixture {
    pub fn validate(&self) -> Result<(), FixtureError> {
        self.config.validate()?;
        let mut scopes = BTreeSet::from(["global".to_string()]);
        scopes.extend(
            self.config
                .projects
                .iter()
                .map(|p| format!("project:{}", p.name)),
        );
        scopes.extend(self.config.links.iter().map(|l| format!("link:{}", l.name)));
        let scope_check = |owner: &str, scope: &str| {
            if scopes.contains(scope) {
                Ok(())
            } else {
                Err(FixtureError::UnknownScope {
                    owner: owner.into(),
                    scope: scope.into(),
                })
            }
        };
        let mut items = BTreeMap::new();
        for item in &self.items {
            if item.id.trim().is_empty() {
                return Err(FixtureError::EmptyId {
                    owner: "item".into(),
                    field: "id",
                });
            }
            if items.insert(item.id.as_str(), item).is_some() {
                return Err(FixtureError::DuplicateId {
                    entity: "item",
                    id: item.id.clone(),
                });
            }
            scope_check(&item.id, &item.scope)?;
            if item.kind == Kind::Law {
                if item.origin != Origin::User {
                    return Err(FixtureError::LawOrigin(item.id.clone()));
                }
                if item.triggers.always {
                    return Err(FixtureError::LawAlways(item.id.clone()));
                }
            }
            if item.kind == Kind::Law && item.body.len() > 400 {
                return Err(FixtureError::LawTooLong {
                    id: item.id.clone(),
                    bytes: item.body.len(),
                });
            }
        }
        let reference_check = |owner: &str, field: &'static str, id: &str| {
            if items.contains_key(id) {
                Ok(())
            } else {
                Err(FixtureError::UnknownItem {
                    owner: owner.into(),
                    field,
                    id: id.into(),
                })
            }
        };
        for item in &self.items {
            if let Some(id) = &item.superseded_by {
                reference_check(&item.id, "superseded_by", id)?;
                let successor = items[id.as_str()];
                if successor.kind != item.kind {
                    return Err(FixtureError::SupersessionKind(item.id.clone()));
                }
                if successor.scope != item.scope {
                    return Err(FixtureError::SupersessionScope(item.id.clone()));
                }
            }
        }
        for item in &self.items {
            if item.superseded_by.is_some() != (item.status == Status::Superseded) {
                return Err(FixtureError::SupersessionStatus(item.id.clone()));
            }
        }
        for item in &self.items {
            let mut seen = BTreeSet::new();
            let mut current = Some(item.id.as_str());
            while let Some(id) = current {
                if !seen.insert(id) {
                    return Err(FixtureError::SupersessionCycle(id.into()));
                }
                let node = items[id];
                current = node.superseded_by.as_deref();
                if item.superseded_by.is_some()
                    && current.is_none()
                    && node.status != Status::Active
                {
                    return Err(FixtureError::SupersessionInactiveEnd(id.into()));
                }
            }
        }
        let mut events = BTreeSet::new();
        let mut prompts = BTreeMap::new();
        for event in &self.events {
            for (field, value) in [("id", &event.id), ("session_id", &event.session_id)] {
                if value.trim().is_empty() {
                    return Err(FixtureError::EmptyId {
                        owner: event.id.clone(),
                        field,
                    });
                }
            }
            for (field, values) in [
                ("required", &event.required),
                ("forbidden", &event.forbidden),
                ("allowed_scopes", &event.allowed_scopes),
            ] {
                let mut seen = BTreeSet::new();
                for value in values {
                    if !seen.insert(value) {
                        return Err(FixtureError::DuplicateEntry {
                            event: event.id.clone(),
                            field,
                            value: value.clone(),
                        });
                    }
                }
            }
            if event.prompt.is_none() && event.tool_paths.as_ref().is_none_or(Vec::is_empty) {
                return Err(FixtureError::MissingToolPaths(event.id.clone()));
            }
            if !events.insert(event.id.as_str()) {
                return Err(FixtureError::DuplicateId {
                    entity: "event",
                    id: event.id.clone(),
                });
            }
            if event.family.trim().is_empty() {
                return Err(FixtureError::EmptyFamily(event.id.clone()));
            }
            if let Some(prompt) = &event.prompt {
                if prompt.trim().is_empty() {
                    return Err(FixtureError::EmptyPrompt(event.id.clone()));
                }
                let normalized = prompt
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if let Some(first) = prompts.insert(normalized, event.id.clone()) {
                    return Err(FixtureError::DuplicatePrompt {
                        first,
                        second: event.id.clone(),
                    });
                }
            }
            for scope in &event.allowed_scopes {
                scope_check(&event.id, scope)?;
            }
            for (field, ids) in [
                ("required", &event.required),
                ("forbidden", &event.forbidden),
            ] {
                for id in ids {
                    reference_check(&event.id, field, id)?;
                }
            }
            for id in &event.required {
                let item = items[id.as_str()];
                if !event.allowed_scopes.contains(&item.scope) {
                    return Err(FixtureError::RequiredScope {
                        event: event.id.clone(),
                        id: id.clone(),
                    });
                }
                if item.status != Status::Active {
                    return Err(FixtureError::RequiredInactive {
                        event: event.id.clone(),
                        id: id.clone(),
                    });
                }
                if let Some(expires_at) = &item.expires_at {
                    let timestamp = |owner: &str, field, value: &str| {
                        chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
                            FixtureError::InvalidTimestamp {
                                owner: owner.into(),
                                field,
                                value: value.into(),
                            }
                        })
                    };
                    if timestamp(&item.id, "expires_at", expires_at)?
                        <= timestamp(&event.id, "now", &event.now)?
                    {
                        return Err(FixtureError::RequiredExpired {
                            event: event.id.clone(),
                            id: id.clone(),
                        });
                    }
                }
                if event.forbidden.contains(id) {
                    return Err(FixtureError::LabelOverlap {
                        event: event.id.clone(),
                        id: id.clone(),
                    });
                }
            }
        }
        let mut covered = BTreeSet::new();
        for id in self.split.dev.iter().chain(&self.split.test) {
            if !events.contains(id.as_str()) {
                return Err(FixtureError::UnknownSplitEvent(id.clone()));
            }
            if !covered.insert(id.as_str()) {
                return Err(FixtureError::DuplicateSplitEvent(id.clone()));
            }
        }
        if let Some(id) = events.difference(&covered).next() {
            return Err(FixtureError::MissingSplitEvent((*id).into()));
        }
        let dev: BTreeSet<_> = self.split.dev.iter().collect();
        let mut sessions = BTreeMap::new();
        let mut families = BTreeMap::new();
        for event in &self.events {
            let side = dev.contains(&event.id);
            for (entity, id, groups) in [
                ("session", &event.session_id, &mut sessions),
                ("family", &event.family, &mut families),
            ] {
                if let Some(previous) = groups.insert(id, side) {
                    if previous != side {
                        return Err(FixtureError::SplitLeak {
                            entity,
                            id: id.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}
