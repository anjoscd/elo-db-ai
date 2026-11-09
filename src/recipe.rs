//! Recipes: procedures that worked, kept as reusable steps (CODESKILL, arXiv 2605.25430).
//!
//! A recipe is a note whose id starts with `receita-` and whose body is a numbered list.
//! It is delivered like any note (by its keywords, or by a file it names), so the agent
//! gets "how we do this here" at the moment it is about to do it.
use crate::store::{Store, StoreError};
use crate::write::{add, NewEntry, WriteError};

pub const PREFIX: &str = "receita-";

/// Slug for the id: ascii lowercase words joined by '-', at most 48 chars.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'á' | 'à' | 'â' | 'ã' => 'a',
            'é' | 'ê' => 'e',
            'í' => 'i',
            'ó' | 'ô' | 'õ' => 'o',
            'ú' => 'u',
            'ç' => 'c',
            other => other,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(48).collect();
    out.trim_matches('-').to_string()
}

/// Body: the title and the steps, numbered.
pub fn body(title: &str, steps: &[String]) -> String {
    let mut text = format!("Recipe: {}", title.trim());
    for (n, step) in steps
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        text.push_str(&format!("\n{}. {step}", n + 1));
    }
    text
}

/// Adds a recipe. At least two steps; keywords decide when it is delivered.
pub fn add_recipe(
    store: &mut Store,
    title: &str,
    steps: &[String],
    keywords: &[String],
    as_user: bool,
) -> Result<String, WriteError> {
    let steps: Vec<String> = steps
        .iter()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    if steps.len() < 2 {
        return Err(WriteError::Refused(
            "a recipe needs at least 2 steps (--step ... --step ...)".into(),
        ));
    }
    if title.trim().is_empty() {
        return Err(WriteError::Refused("empty title".into()));
    }
    let mut base = slug(title);
    if base.is_empty() {
        // Titles made only of symbols or other alphabets still get a stable id.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in title.trim().bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        base = format!("{:08x}", h as u32);
    }
    let id = format!("{PREFIX}{base}");
    let exists: bool = store
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
            [&id],
            |r| r.get(0),
        )
        .map_err(WriteError::Sql)?;
    if exists {
        return Err(WriteError::Refused(format!(
            "recipe {id} already exists: use another title or elo supersede"
        )));
    }
    add(
        store,
        &NewEntry {
            kind: "note".into(),
            scope: "global".into(),
            title: title.trim().into(),
            body: body(title, &steps),
            triggers: keywords.to_vec(),
            importance: 0.7,
            supersedes: None,
            expires_at: None,
            id: Some(id),
            as_user,
        },
    )
}

/// `(id, title)` of the active recipes.
pub fn list(store: &Store) -> Result<Vec<(String, String)>, StoreError> {
    let mut stmt = store.conn.prepare(
        "SELECT id, title FROM items WHERE id LIKE 'receita-%' AND status = 'active' ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
