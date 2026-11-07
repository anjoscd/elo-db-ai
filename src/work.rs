//! Projects, tasks, milestones and handoffs, kept in `.elo/work.db`
//! (a database separate from `elo.db`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::config::Config;
use crate::store::DATA_DIR;

pub const WORK_DB: &str = "work.db";
const DAY: i64 = 86_400;
/// Hard ceiling of the session-start block, in bytes.
pub const BLOCK_MAX_BYTES: usize = 600;

pub const STATUSES: [&str; 8] = [
    "backlog",
    "todo",
    "in_progress",
    "blocked",
    "deferred",
    "in_review",
    "completed",
    "archived",
];
/// Statuses that go stale when left untouched.
const OPEN: [&str; 5] = ["backlog", "todo", "in_progress", "blocked", "in_review"];

#[derive(Debug, thiserror::Error)]
pub enum WorkError {
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Msg(String),
}

fn msg<T>(text: impl Into<String>) -> Result<T, WorkError> {
    Err(WorkError::Msg(text.into()))
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Project {
    pub slug: String,
    pub name: String,
    pub path: String,
    pub open_tasks: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Milestone {
    pub project: String,
    pub slug: String,
    pub name: String,
    pub done: i64,
    pub total: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub id: String,
    pub project: String,
    pub text: String,
    pub milestone: Option<String>,
    /// Stored status; a stale open task reports `deferred`.
    pub status: String,
    pub stale: bool,
    pub blocked_by: Vec<String>,
    pub idle_days: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Handoff {
    pub id: String,
    pub project: String,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub stale: bool,
    pub idle_days: i64,
}

pub struct Work {
    conn: Connection,
    stale_days: u32,
}

/// `.elo/work.db` under `root`.
pub fn db_path(root: &Path) -> PathBuf {
    root.join(DATA_DIR).join(WORK_DB)
}

/// `elo.toml` when present and valid, otherwise the defaults.
fn config_of(root: &Path) -> Config {
    if root.join("elo.toml").is_file() {
        Config::load(root).unwrap_or_default()
    } else {
        Config::default()
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects(
  slug TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS milestones(
  project TEXT NOT NULL REFERENCES projects(slug), slug TEXT NOT NULL, name TEXT NOT NULL,
  created_at INTEGER NOT NULL, PRIMARY KEY(project, slug));
CREATE TABLE IF NOT EXISTS tasks(
  n INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL REFERENCES projects(slug),
  text TEXT NOT NULL, milestone TEXT, status TEXT NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, completed_at INTEGER);
CREATE TABLE IF NOT EXISTS task_blocks(
  task INTEGER NOT NULL REFERENCES tasks(n), blocker INTEGER NOT NULL REFERENCES tasks(n),
  PRIMARY KEY(task, blocker));
CREATE INDEX IF NOT EXISTS task_blocks_blocker ON task_blocks(blocker);
CREATE TABLE IF NOT EXISTS handoffs(
  n INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL REFERENCES projects(slug),
  title TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
";

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 40
        && slug != "."
        && slug != ".."
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// `t12`, `T12` or `12` give 12 for prefix `t`.
fn parse_id(text: &str, prefix: char) -> Option<i64> {
    let t = text.trim();
    let t = t
        .strip_prefix(prefix)
        .or_else(|| t.strip_prefix(prefix.to_ascii_uppercase()))
        .unwrap_or(t);
    t.parse().ok().filter(|n| *n > 0)
}

fn number(id: &str) -> i64 {
    id.chars()
        .next()
        .and_then(|c| id.get(c.len_utf8()..))
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

impl Work {
    /// Opens (creating when needed) `.elo/work.db` for writing.
    pub fn open(root: &Path) -> Result<Self, WorkError> {
        std::fs::create_dir_all(root.join(DATA_DIR)).map_err(|e| WorkError::Msg(e.to_string()))?;
        let conn = Connection::open(db_path(root))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Rollback journal only: no -wal/-shm files, and a read-only open never writes.
        conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            stale_days: config_of(root).work.stale_days,
        })
    }

    /// Opens an existing `work.db` read-only; `None` when it does not exist.
    pub fn open_read_only(root: &Path, stale_days: u32) -> Option<Self> {
        let path = db_path(root);
        if !path.is_file() {
            return None;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        conn.busy_timeout(std::time::Duration::from_millis(200))
            .ok()?;
        Some(Self { conn, stale_days })
    }

    fn cutoff(&self) -> Option<i64> {
        (self.stale_days > 0).then(|| now() - i64::from(self.stale_days) * DAY)
    }

    fn is_stale(&self, status: &str, updated: i64) -> bool {
        self.cutoff().is_some_and(|c| updated < c) && OPEN.contains(&status)
    }

    // ---- projects ----

    pub fn add_project(
        &self,
        slug: &str,
        path: &Path,
        name: Option<&str>,
    ) -> Result<Project, WorkError> {
        if !valid_slug(slug) {
            return msg(format!(
                "invalid slug: {slug:?} (use letters, digits, - _ .)"
            ));
        }
        let absolute = absolute_path(path);
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO projects(slug,name,path,created_at) VALUES(?,?,?,?)",
            params![
                slug,
                name.unwrap_or(slug),
                absolute.to_string_lossy(),
                now()
            ],
        )?;
        if changed == 0 {
            return msg(format!("project {slug} already exists"));
        }
        Ok(Project {
            slug: slug.into(),
            name: name.unwrap_or(slug).into(),
            path: absolute.to_string_lossy().into_owned(),
            open_tasks: 0,
        })
    }

    pub fn projects(&self) -> Result<Vec<Project>, WorkError> {
        let mut st = self.conn.prepare(
            "SELECT p.slug,p.name,p.path,
              (SELECT COUNT(*) FROM tasks t WHERE t.project=p.slug
                 AND t.status IN ('backlog','todo','in_progress','blocked','in_review'))
             FROM projects p ORDER BY p.slug",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Project {
                slug: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                open_tasks: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn require_project(&self, slug: &str) -> Result<(), WorkError> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM projects WHERE slug=?", [slug], |r| r.get(0))
            .optional()?;
        if found.is_none() {
            return msg(format!("project {slug} does not exist (elo project add)"));
        }
        Ok(())
    }

    // ---- milestones ----

    pub fn add_milestone(&self, project: &str, slug: &str, name: &str) -> Result<(), WorkError> {
        self.require_project(project)?;
        if !valid_slug(slug) {
            return msg(format!("invalid milestone slug: {slug:?}"));
        }
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO milestones(project,slug,name,created_at) VALUES(?,?,?,?)",
            params![project, slug, name, now()],
        )?;
        if changed == 0 {
            return msg(format!("milestone {slug} already exists in {project}"));
        }
        Ok(())
    }

    pub fn milestones(&self, project: Option<&str>) -> Result<Vec<Milestone>, WorkError> {
        let mut st = self.conn.prepare(
            "SELECT m.project,m.slug,m.name,
              (SELECT COUNT(*) FROM tasks t WHERE t.project=m.project AND t.milestone=m.slug
                 AND t.status='completed'),
              (SELECT COUNT(*) FROM tasks t WHERE t.project=m.project AND t.milestone=m.slug
                 AND t.status<>'archived')
             FROM milestones m WHERE (?1 IS NULL OR m.project=?1)
             ORDER BY m.project,m.created_at,m.slug",
        )?;
        let rows = st.query_map([project], |r| {
            Ok(Milestone {
                project: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                done: r.get(3)?,
                total: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- tasks ----

    pub fn add_task(
        &mut self,
        project: &str,
        text: &str,
        milestone: Option<&str>,
        blocked_by: &[String],
    ) -> Result<Task, WorkError> {
        self.require_project(project)?;
        if text.trim().is_empty() {
            return msg("task text is empty");
        }
        if let Some(m) = milestone {
            let found: Option<i64> = self
                .conn
                .query_row(
                    "SELECT 1 FROM milestones WHERE project=? AND slug=?",
                    params![project, m],
                    |r| r.get(0),
                )
                .optional()?;
            if found.is_none() {
                return msg(format!(
                    "milestone {m} does not exist in {project} (elo milestone add)"
                ));
            }
        }
        let mut wanted = Vec::new();
        for b in blocked_by {
            let Some(n) = parse_id(b, 't') else {
                return msg(format!("invalid task id: {b}"));
            };
            wanted.push(n);
        }
        let t = now();
        let tx = self.conn.transaction()?;
        let next: i64 = tx.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='tasks'),0)+1",
            [],
            |r| r.get(0),
        )?;
        let mut open_blockers = false;
        for n in &wanted {
            if *n == next {
                return msg(format!("task t{n} cannot block itself"));
            }
            let status: Option<String> = tx
                .query_row("SELECT status FROM tasks WHERE n=?", [n], |r| r.get(0))
                .optional()?;
            match status {
                None => return msg(format!("task t{n} does not exist")),
                Some(s) => open_blockers |= !["completed", "archived"].contains(&s.as_str()),
            }
        }
        let status = if open_blockers { "blocked" } else { "todo" };
        tx.execute(
            "INSERT INTO tasks(project,text,milestone,status,created_at,updated_at)
             VALUES(?,?,?,?,?,?)",
            params![project, text.trim(), milestone, status, t, t],
        )?;
        let n = tx.last_insert_rowid();
        for b in &wanted {
            tx.execute(
                "INSERT OR IGNORE INTO task_blocks(task,blocker) VALUES(?,?)",
                params![n, b],
            )?;
        }
        tx.commit()?;
        self.task(n)
    }

    fn task_from(&self, r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
        let n: i64 = r.get(0)?;
        let stored: String = r.get(4)?;
        let updated: i64 = r.get(5)?;
        let stale = self.is_stale(&stored, updated);
        Ok(Task {
            id: format!("t{n}"),
            project: r.get(1)?,
            text: r.get(2)?,
            milestone: r.get(3)?,
            status: if stale { "deferred".into() } else { stored },
            stale,
            blocked_by: Vec::new(),
            idle_days: ((now() - updated) / DAY).max(0),
        })
    }

    /// Open blockers of every task of `project` (all projects when `None`), in one query.
    fn fill_blockers(&self, project: Option<&str>, tasks: &mut [Task]) -> Result<(), WorkError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let mut st = self.conn.prepare(
            "SELECT b.task,b.blocker FROM task_blocks b
             JOIN tasks x ON x.n=b.blocker JOIN tasks t ON t.n=b.task
             WHERE x.status NOT IN ('completed','archived') AND (?1 IS NULL OR t.project=?1)
             ORDER BY b.task,b.blocker",
        )?;
        let mut by_task: HashMap<i64, Vec<String>> = HashMap::new();
        for row in st.query_map([project], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (task, blocker) = row?;
            by_task.entry(task).or_default().push(format!("t{blocker}"));
        }
        for t in tasks {
            t.blocked_by = by_task.remove(&number(&t.id)).unwrap_or_default();
        }
        Ok(())
    }

    const TASK_COLS: &'static str = "n,project,text,milestone,status,updated_at";

    pub fn task(&self, n: i64) -> Result<Task, WorkError> {
        let t = self
            .conn
            .query_row(
                &format!("SELECT {} FROM tasks WHERE n=?", Self::TASK_COLS),
                [n],
                |r| self.task_from(r),
            )
            .optional()?;
        let Some(t) = t else {
            return msg(format!("task t{n} does not exist"));
        };
        let project = t.project.clone();
        let mut one = [t];
        self.fill_blockers(Some(&project), &mut one)?;
        let [t] = one;
        Ok(t)
    }

    fn cutoff_or_min(&self) -> i64 {
        self.cutoff().unwrap_or(i64::MIN)
    }

    /// Default view = open and not deferred; `all` = everything;
    /// `deferred` = only deferred (explicit or stale).
    pub fn tasks(
        &self,
        project: Option<&str>,
        all: bool,
        deferred: bool,
    ) -> Result<Vec<Task>, WorkError> {
        let filter = if all {
            "1"
        } else if deferred {
            "(status='deferred' OR (status IN ('backlog','todo','in_progress','blocked','in_review')
               AND updated_at<?2))"
        } else {
            "(status IN ('backlog','todo','in_progress','blocked','in_review') AND updated_at>=?2)"
        };
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM tasks WHERE (?1 IS NULL OR project=?1) AND {filter}
             AND ?2 IS NOT NULL ORDER BY n",
            Self::TASK_COLS
        ))?;
        let mut out = st
            .query_map(params![project, self.cutoff_or_min()], |r| {
                self.task_from(r)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        self.fill_blockers(project, &mut out)?;
        Ok(out)
    }

    /// Sets a status and returns the tasks this change unblocked.
    pub fn set_status(&mut self, id: &str, status: &str) -> Result<(Task, Vec<Task>), WorkError> {
        if !STATUSES.contains(&status) {
            return msg(format!("invalid status: {status} ({})", STATUSES.join("|")));
        }
        let Some(n) = parse_id(id, 't') else {
            return msg(format!("invalid task id: {id}"));
        };
        let before = self.task(n)?;
        let closed = |s: &str| s == "completed" || s == "archived";
        if matches!(status, "todo" | "in_progress") {
            let open: Vec<i64> = {
                let mut st = self.conn.prepare(
                    "SELECT b.blocker FROM task_blocks b JOIN tasks x ON x.n=b.blocker
                     WHERE b.task=? AND x.status NOT IN ('completed','archived')
                     ORDER BY b.blocker",
                )?;
                let rows = st.query_map([n], |r| r.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            if !open.is_empty() {
                let ids: Vec<String> = open.iter().map(|b| format!("t{b}")).collect();
                return msg(format!(
                    "t{n} is blocked by {} (still open); finish it first or use --status blocked",
                    ids.join(",")
                ));
            }
        }
        let t = now();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE tasks SET status=?1, updated_at=?2,
               completed_at=CASE WHEN ?1='completed' THEN ?2 ELSE NULL END WHERE n=?3",
            params![status, t, n],
        )?;
        let mut freed = Vec::new();
        if closed(&before.status) && !closed(status) {
            tx.execute(
                "UPDATE tasks SET status='blocked', updated_at=?1
                 WHERE status='todo' AND n IN (SELECT task FROM task_blocks WHERE blocker=?2)",
                params![t, n],
            )?;
        }
        if status == "completed" || status == "archived" {
            let mut st = tx.prepare(
                "SELECT b.task FROM task_blocks b JOIN tasks d ON d.n=b.task
                 WHERE b.blocker=? AND d.status='blocked' ORDER BY b.task",
            )?;
            let candidates: Vec<i64> =
                st.query_map([n], |r| r.get(0))?.collect::<Result<_, _>>()?;
            drop(st);
            for d in candidates {
                let waiting: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM task_blocks b JOIN tasks x ON x.n=b.blocker
                     WHERE b.task=? AND x.status NOT IN ('completed','archived')",
                    [d],
                    |r| r.get(0),
                )?;
                if waiting == 0 {
                    tx.execute(
                        "UPDATE tasks SET status='todo', updated_at=? WHERE n=?",
                        params![t, d],
                    )?;
                    freed.push(d);
                }
            }
        }
        tx.commit()?;
        let freed = freed
            .into_iter()
            .map(|d| self.task(d))
            .collect::<Result<_, _>>()?;
        Ok((self.task(n)?, freed))
    }

    // ---- handoffs ----

    pub fn new_handoff(
        &self,
        project: &str,
        title: &str,
        body: &str,
    ) -> Result<Handoff, WorkError> {
        self.require_project(project)?;
        if title.trim().is_empty() {
            return msg("handoff title is empty");
        }
        let t = now();
        self.conn.execute(
            "INSERT INTO handoffs(project,title,body,status,created_at,updated_at)
             VALUES(?,?,?,'open',?,?)",
            params![project, title.trim(), body, t, t],
        )?;
        self.handoff(self.conn.last_insert_rowid())
    }

    const HANDOFF_COLS: &'static str = "n,project,title,body,status,updated_at";

    fn handoff_from(&self, r: &rusqlite::Row<'_>) -> rusqlite::Result<Handoff> {
        let n: i64 = r.get(0)?;
        let status: String = r.get(4)?;
        let updated: i64 = r.get(5)?;
        Ok(Handoff {
            id: format!("h{n}"),
            project: r.get(1)?,
            title: r.get(2)?,
            body: r.get(3)?,
            open: status == "open",
            stale: status == "open" && self.cutoff().is_some_and(|c| updated < c),
            idle_days: ((now() - updated) / DAY).max(0),
        })
    }

    pub fn handoff(&self, n: i64) -> Result<Handoff, WorkError> {
        self.conn
            .query_row(
                &format!("SELECT {} FROM handoffs WHERE n=?", Self::HANDOFF_COLS),
                [n],
                |r| self.handoff_from(r),
            )
            .optional()?
            .map_or_else(|| msg(format!("handoff h{n} does not exist")), Ok)
    }

    /// Default view = open and not deferred; `all` = everything; `deferred` = stale open ones.
    pub fn handoffs(
        &self,
        project: Option<&str>,
        all: bool,
        deferred: bool,
    ) -> Result<Vec<Handoff>, WorkError> {
        let filter = if all {
            "1"
        } else if deferred {
            "(status='open' AND updated_at<?2)"
        } else {
            "(status='open' AND updated_at>=?2)"
        };
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM handoffs WHERE (?1 IS NULL OR project=?1) AND {filter}
             AND ?2 IS NOT NULL ORDER BY n",
            Self::HANDOFF_COLS
        ))?;
        let rows = st.query_map(params![project, self.cutoff_or_min()], |r| {
            self.handoff_from(r)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// An id (`h3`, `3`) or a text found in the title or body (newest match wins).
    pub fn find_handoff(&self, key: &str) -> Result<Handoff, WorkError> {
        if let Some(n) = parse_id(key, 'h') {
            return self.handoff(n);
        }
        let needle = key.trim().to_lowercase();
        if needle.is_empty() {
            return msg("no handoff given");
        }
        self.handoffs(None, true, false)?
            .into_iter()
            .rev()
            .find(|h| {
                h.title.to_lowercase().contains(&needle) || h.body.to_lowercase().contains(&needle)
            })
            .map_or_else(|| msg(format!("no handoff matching {key:?}")), Ok)
    }

    pub fn close_handoff(&self, key: &str) -> Result<Handoff, WorkError> {
        let h = self.find_handoff(key)?;
        self.conn.execute(
            "UPDATE handoffs SET status='closed', updated_at=? WHERE n=?",
            params![now(), number(&h.id)],
        )?;
        self.handoff(number(&h.id))
    }

    // ---- active project and conversation-start block ----

    /// Work project of the folder `cwd`: the most specific registered path holding it
    /// (both sides normalized the same way), else the `elo.toml` project (existing
    /// resolution) whose name is a registered slug.
    fn active_project(&self, root: &Path, cwd: &Path, config: &Config) -> Option<String> {
        let mut st = self.conn.prepare("SELECT slug,path FROM projects").ok()?;
        let rows: Vec<(String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()?
            .filter_map(Result::ok)
            .collect();
        let here = key(cwd);
        let home = home_dir().map(|h| key(&h));
        let by_path = rows
            .iter()
            .filter_map(|(slug, p)| {
                let k = key(Path::new(p));
                // "/" and the home folder only match themselves, never their subfolders.
                // A drive root (Unix "/" or a Windows drive) has no normal component.
                let is_root = Path::new(p)
                    .components()
                    .all(|c| !matches!(c, std::path::Component::Normal(_)));
                let broad = is_root || k.len() <= 1 || home.as_ref() == Some(&k);
                let holds = if broad {
                    here == k
                } else {
                    here.starts_with(&k)
                };
                holds.then_some((k.len(), slug))
            })
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)));
        if let Some((_, slug)) = by_path {
            return Some(slug.clone());
        }
        let cwd = absolute_path(cwd);
        let rel = PathBuf::from(crate::hook::relative(root, &cwd, &cwd, &[]));
        config
            .projects
            .iter()
            .filter(|p| {
                p.root
                    .components()
                    .any(|c| matches!(c, Component::Normal(_)))
                    && rel.starts_with(&p.root)
            })
            .max_by_key(|p| p.root.components().count())
            .filter(|p| rows.iter().any(|(s, _)| *s == p.name))
            .map(|p| p.name.clone())
    }

    /// At most three queries: handoffs, tasks, blockers.
    fn block(&self, slug: &str) -> Result<Option<String>, WorkError> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM handoffs WHERE project=?1 AND status='open' ORDER BY n",
            Self::HANDOFF_COLS
        ))?;
        let (handoffs, late): (Vec<_>, Vec<_>) = st
            .query_map([slug], |r| self.handoff_from(r))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .partition(|h| !h.stale);
        let late_handoffs = late.len();
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM tasks WHERE project=?1
             AND status IN ('backlog','todo','in_progress','blocked','in_review','deferred')
             ORDER BY n",
            Self::TASK_COLS
        ))?;
        let mut doing = Vec::new();
        let mut blocked = Vec::new();
        let mut late_tasks = 0;
        for t in st.query_map([slug], |r| self.task_from(r))? {
            let t = t?;
            match t.status.as_str() {
                "deferred" => late_tasks += 1,
                "in_progress" => doing.push(t),
                "blocked" => blocked.push(t),
                _ => {}
            }
        }
        if handoffs.is_empty()
            && doing.is_empty()
            && blocked.is_empty()
            && late_tasks == 0
            && late_handoffs == 0
        {
            return Ok(None);
        }
        self.fill_blockers(Some(slug), &mut blocked)?;
        let mut lines = vec![format!("ELO work [{slug}]")];
        for h in &handoffs {
            lines.push(format!("handoff {}: {}", h.id, clip(&h.title, 70)));
        }
        for t in &doing {
            lines.push(format!("doing {}: {}", t.id, clip(&t.text, 70)));
        }
        for t in &blocked {
            let by = t.blocked_by.join(",");
            lines.push(format!(
                "blocked {}: {} (waiting on {by})",
                t.id,
                clip(&t.text, 60)
            ));
        }
        let mut tail = Vec::new();
        if late_tasks > 0 {
            tail.push(format!("{late_tasks} deferred: elo task list --deferred"));
        }
        if late_handoffs > 0 {
            let noun = if late_handoffs == 1 {
                "handoff"
            } else {
                "handoffs"
            };
            tail.push(format!(
                "{late_handoffs} deferred {noun}: elo handoff list --deferred"
            ));
        }
        Ok(Some(fit(lines, &tail)))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Normal form used to compare folders: absolute, symlinks resolved as far as the path
/// exists (the missing tail is kept lexically), components compared without case where
/// the file system ignores it.
fn key(path: &Path) -> Vec<OsString> {
    let abs = absolute_path(path);
    let mut existing = abs.as_path();
    let mut tail = Vec::new();
    let base = loop {
        if let Ok(c) = std::fs::canonicalize(existing) {
            break c;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => break existing.to_path_buf(),
        }
    };
    let mut parts: Vec<OsString> = base
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    parts.extend(tail.into_iter().rev());
    if cfg!(any(target_os = "macos", windows)) {
        for p in &mut parts {
            *p = p.to_string_lossy().to_lowercase().into();
        }
    }
    parts
}

fn absolute_path(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// First line cut at `max` characters, with `…` when cut.
pub fn clip(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut s: String = line.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Header, detail lines, then the footer (the deferred counts). Block plus the final
/// newline stays within `BLOCK_MAX_BYTES`; over that
/// only the details are cut, from the end, replaced by `… +N`; header and footer stay.
fn fit(lines: Vec<String>, footer: &[String]) -> String {
    let Some((head, details)) = lines.split_first() else {
        return footer.join("\n");
    };
    let build = |keep: usize| {
        let mut out = vec![head.clone()];
        out.extend(details[..keep].iter().cloned());
        if keep < details.len() {
            out.push(format!("… +{}", details.len() - keep));
        }
        out.extend(footer.iter().cloned());
        out.join("\n")
    };
    (0..=details.len())
        .rev()
        .map(build)
        .find(|b| b.len() < BLOCK_MAX_BYTES)
        .unwrap_or_else(|| build(0))
}

/// Short block for the start of a conversation: only the active project's open handoffs,
/// in-progress/blocked tasks and the deferred count. `None` when there is nothing open,
/// no `work.db`, or the folder maps to no project. Never writes.
pub fn session_start_block(root: &Path, cwd: &Path) -> Option<String> {
    session_start_project_block(root, cwd).map(|(_, block)| block)
}

/// The project identity and its block are resolved together, using the same rules.
pub(crate) fn session_start_project_block(root: &Path, cwd: &Path) -> Option<(String, String)> {
    if !db_path(root).is_file() {
        return None;
    }
    let config = config_of(root);
    let work = Work::open_read_only(root, config.work.stale_days)?;
    let slug = work.active_project(root, cwd, &config)?;
    let block = work.block(&slug).ok().flatten()?;
    Some((slug, block))
}

/// Command line surface (`elo project|task|milestone|handoff`).
pub mod cli {
    use super::*;
    use clap::Subcommand;

    #[derive(Debug, Subcommand)]
    pub enum ProjectCmd {
        /// Register a project folder.
        Add {
            slug: String,
            #[arg(long)]
            path: PathBuf,
            #[arg(long)]
            name: Option<String>,
            #[arg(long)]
            json: bool,
        },
        /// List projects.
        List {
            #[arg(long)]
            json: bool,
        },
    }

    #[derive(Debug, Subcommand)]
    pub enum TaskCmd {
        /// Add a task.
        Add {
            #[arg(long)]
            project: String,
            text: String,
            #[arg(long)]
            milestone: Option<String>,
            /// Task id that must finish first (repeatable).
            #[arg(long = "blocked-by")]
            blocked_by: Vec<String>,
            #[arg(long)]
            json: bool,
        },
        /// List open tasks.
        List {
            #[arg(long)]
            project: Option<String>,
            /// Include completed, archived and deferred.
            #[arg(long, conflicts_with = "deferred")]
            all: bool,
            /// Only deferred (set so, or untouched past `[work] stale_days`).
            #[arg(long)]
            deferred: bool,
            #[arg(long)]
            json: bool,
        },
        /// Complete a task and list what it unblocked.
        Done {
            id: String,
            #[arg(long)]
            json: bool,
        },
        /// Change a task status.
        Update {
            id: String,
            /// backlog|todo|in_progress|blocked|deferred|in_review|completed|archived
            #[arg(long)]
            status: String,
            #[arg(long)]
            json: bool,
        },
    }

    #[derive(Debug, Subcommand)]
    pub enum MilestoneCmd {
        /// Add a milestone to a project.
        Add {
            #[arg(long)]
            project: String,
            slug: String,
            name: String,
            #[arg(long)]
            json: bool,
        },
        /// List milestones with progress.
        List {
            #[arg(long)]
            project: Option<String>,
            #[arg(long)]
            json: bool,
        },
    }

    #[derive(Debug, Subcommand)]
    pub enum HandoffCmd {
        /// Leave a handoff for the next conversation.
        New {
            #[arg(long)]
            project: String,
            #[arg(long)]
            title: String,
            #[arg(long)]
            body: String,
            #[arg(long)]
            json: bool,
        },
        /// List open handoffs.
        List {
            #[arg(long)]
            project: Option<String>,
            #[arg(long, conflicts_with = "deferred")]
            all: bool,
            #[arg(long)]
            deferred: bool,
            #[arg(long)]
            json: bool,
        },
        /// Show a handoff by id or by text in its title/body.
        Show {
            key: String,
            #[arg(long)]
            json: bool,
        },
        /// Close a handoff.
        Close {
            key: String,
            #[arg(long)]
            json: bool,
        },
    }

    fn json<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap_or_default()
    }

    fn task_line(t: &Task) -> String {
        let mut s = format!("{} {} [{}] {}", t.id, t.status, t.project, t.text);
        if let Some(m) = &t.milestone {
            s.push_str(&format!(" (milestone {m})"));
        }
        if !t.blocked_by.is_empty() {
            s.push_str(&format!(" (waiting on {})", t.blocked_by.join(",")));
        }
        if t.stale {
            s.push_str(&format!(" (idle {}d)", t.idle_days));
        }
        s
    }

    fn lines<T>(items: &[T], empty: &str, f: impl Fn(&T) -> String) -> String {
        if items.is_empty() {
            return empty.to_string();
        }
        items.iter().map(f).collect::<Vec<_>>().join("\n")
    }

    fn handoff_line(h: &Handoff) -> String {
        let state = match (h.open, h.stale) {
            (false, _) => " (closed)".to_string(),
            (true, true) => format!(" (idle {}d)", h.idle_days),
            _ => String::new(),
        };
        format!("{} [{}] {}{state}", h.id, h.project, clip(&h.title, 80))
    }

    pub fn project(root: &Path, cmd: ProjectCmd) -> Result<String, WorkError> {
        let work = Work::open(root)?;
        Ok(match cmd {
            ProjectCmd::Add {
                slug,
                path,
                name,
                json: j,
            } => {
                let path = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                let p = work.add_project(&slug, &path, name.as_deref())?;
                if j {
                    json(&p)
                } else {
                    format!("project {} -> {}", p.slug, p.path)
                }
            }
            ProjectCmd::List { json: j } => {
                let all = work.projects()?;
                if j {
                    json(&all)
                } else {
                    lines(&all, "no projects", |p| {
                        format!("{} {} open {}", p.slug, p.open_tasks, p.path)
                    })
                }
            }
        })
    }

    pub fn task(root: &Path, cmd: TaskCmd) -> Result<String, WorkError> {
        let mut work = Work::open(root)?;
        Ok(match cmd {
            TaskCmd::Add {
                project,
                text,
                milestone,
                blocked_by,
                json: j,
            } => {
                let t = work.add_task(&project, &text, milestone.as_deref(), &blocked_by)?;
                if j {
                    json(&t)
                } else {
                    task_line(&t)
                }
            }
            TaskCmd::List {
                project,
                all,
                deferred,
                json: j,
            } => {
                let found = work.tasks(project.as_deref(), all, deferred)?;
                if j {
                    json(&found)
                } else {
                    lines(&found, "no tasks", task_line)
                }
            }
            TaskCmd::Done { id, json: j } => finish(&mut work, &id, "completed", j)?,
            TaskCmd::Update {
                id,
                status,
                json: j,
            } => finish(&mut work, &id, &status, j)?,
        })
    }

    fn finish(work: &mut Work, id: &str, status: &str, j: bool) -> Result<String, WorkError> {
        let (t, freed) = work.set_status(id, status)?;
        if j {
            return Ok(serde_json::json!({"task": t, "unblocked": freed}).to_string());
        }
        let mut out = task_line(&t);
        for f in &freed {
            out.push_str(&format!("\nunblocked {} {}", f.id, f.text));
        }
        Ok(out)
    }

    pub fn milestone(root: &Path, cmd: MilestoneCmd) -> Result<String, WorkError> {
        let work = Work::open(root)?;
        Ok(match cmd {
            MilestoneCmd::Add {
                project,
                slug,
                name,
                json: j,
            } => {
                work.add_milestone(&project, &slug, &name)?;
                if j {
                    serde_json::json!({"project": project, "slug": slug, "name": name}).to_string()
                } else {
                    format!("milestone {slug} [{project}] {name}")
                }
            }
            MilestoneCmd::List { project, json: j } => {
                let all = work.milestones(project.as_deref())?;
                if j {
                    json(&all)
                } else {
                    lines(&all, "no milestones", |m| {
                        format!(
                            "{} [{}] {}/{} {}",
                            m.slug, m.project, m.done, m.total, m.name
                        )
                    })
                }
            }
        })
    }

    pub fn handoff(root: &Path, cmd: HandoffCmd) -> Result<String, WorkError> {
        let work = Work::open(root)?;
        Ok(match cmd {
            HandoffCmd::New {
                project,
                title,
                body,
                json: j,
            } => {
                let h = work.new_handoff(&project, &title, &body)?;
                if j {
                    json(&h)
                } else {
                    handoff_line(&h)
                }
            }
            HandoffCmd::List {
                project,
                all,
                deferred,
                json: j,
            } => {
                let found = work.handoffs(project.as_deref(), all, deferred)?;
                if j {
                    json(&found)
                } else {
                    lines(&found, "no handoffs", handoff_line)
                }
            }
            HandoffCmd::Show { key, json: j } => {
                let h = work.find_handoff(&key)?;
                if j {
                    json(&h)
                } else {
                    format!("{}\n{}", handoff_line(&h), h.body)
                }
            }
            HandoffCmd::Close { key, json: j } => {
                let h = work.close_handoff(&key)?;
                if j {
                    json(&h)
                } else {
                    handoff_line(&h)
                }
            }
        })
    }
}
