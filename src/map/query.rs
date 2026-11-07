//! Queries over the code map.

use std::io::Write;
use std::path::Path;

use super::store::MapStore;
use super::{Entity, MapError};

pub struct Report {
    pub hits: Vec<Entity>,
    pub indexed: usize,
    pub stale: usize,
}

pub fn file(root: &Path, file: &str) -> Result<Report, MapError> {
    let store = MapStore::open_read(root)?;
    let hits = store.entities_for_file(file)?;
    let (indexed, stale) = store.counts(root)?;
    Ok(Report {
        hits,
        indexed,
        stale,
    })
}

pub fn contains(root: &Path, name: &str) -> Result<Report, MapError> {
    let store = MapStore::open_read(root)?;
    let hits = store.entities_containing(name)?;
    let (indexed, stale) = store.counts(root)?;
    Ok(Report {
        hits,
        indexed,
        stale,
    })
}

pub fn print(out: &mut impl Write, report: &Report, json: bool) -> Result<(), MapError> {
    if json {
        let text = serde_json::to_string(&report.hits)
            .map_err(|error| MapError::Json(error.to_string()))?;
        writeln!(out, "{text}")?;
        return Ok(());
    }
    for entity in &report.hits {
        let inside = match &entity.container {
            Some(container) => format!("  (inside {container})"),
            None => String::new(),
        };
        writeln!(
            out,
            "{}:{}  {}  {}{inside}",
            entity.file,
            entity.line,
            entity.kind.as_str(),
            entity.name
        )?;
    }
    writeln!(
        out,
        "{} results · {} files indexed · {} stale",
        report.hits.len(),
        report.indexed,
        report.stale
    )?;
    Ok(())
}
