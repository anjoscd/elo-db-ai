//! Implementation of `elo init`.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::config::CONFIG_FILE;
use crate::store::{self, Store, StoreError};

/// Default `elo.toml`, written only when the file is absent.
pub const CONFIG_TEMPLATE: &str = r#"
# ELO configuration.
# Every section is optional; the values below are the defaults.

[workspace]
# How many bytes of context `elo context` may spend.
budget_bytes = 9000
# Items scoring below this value are dropped from the context.
min_score = 0.35

[quotas]
# Share of the budget per source; the three values must add up to 1.0.
project = 0.50
link = 0.25
global = 0.25

# A project is a folder inside the workspace.
# [[project]]
# name = "api"
# root = "services/api"
# aliases = ["backend"]

# A link connects two or more projects around shared concepts.
# [[link]]
# name = "auth"
# projects = ["api", "web"]
# keywords = ["token", "session"]
# code_signals = ["AuthGuard"]
"#;

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("cannot write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Result of a successful `elo init`.
#[derive(Debug, Clone)]
pub struct InitOutcome {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub config_written: bool,
}

pub fn init(root: &Path) -> Result<InitOutcome, InitError> {
    let data_dir = store::data_dir(root);
    fs::create_dir_all(&data_dir).map_err(|source| InitError::Io {
        path: data_dir.clone(),
        source,
    })?;

    let config_path = root.join(CONFIG_FILE);
    let config_written = write_config(&config_path, root)?;

    Store::open(root)?;

    Ok(InitOutcome {
        data_dir,
        config_path,
        config_written,
    })
}

/// Writes the default configuration when none exists; returns whether it wrote.
fn write_config(path: &Path, root: &Path) -> Result<bool, InitError> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(source) => {
            return Err(InitError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let found = crate::discover::discover(root);
    let mut content = CONFIG_TEMPLATE.to_string();
    if !found.projects.is_empty() {
        content.push_str(
            "\n# Projects discovered from package manifests (SPEC §2.1). Edit to correct.\n",
        );
        content.push_str(&crate::discover::to_toml(&found.projects));
    }
    file.write_all(content.as_bytes())
        .map_err(|source| InitError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(true)
}
