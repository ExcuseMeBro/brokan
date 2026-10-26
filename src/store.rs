//! All reads and writes of `kanzo.db`. Plain SQL, one short-lived connection per operation.
//!
//! The database uses a rollback journal (`journal_mode = TRUNCATE`), not WAL. With a
//! connection per operation, WAL would set up and tear down its `-wal` and `-shm` files
//! on every request; the rollback journal makes a read touch no extra files at all and
//! measured about 3x faster per operation. Writes queue on the busy timeout.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// Schema migrations, applied in order. Index + 1 is the `schema_version` they lead to.
const MIGRATIONS: &[&str] = &[include_str!("schema.sql")];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    /// Another process holds the data folder for longer than we wait, like a long `kanzo sync`.
    Busy(String),
    Db(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) | Error::Invalid(m) | Error::Busy(m) => f.write_str(m),
            Error::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub repo: Option<String>,
    pub created_at: String,
    /// Tasks not in the last (done) column.
    pub open: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub position: f64,
    pub created_at: String,
    pub updated_at: String,
    pub comments: i64,
}

impl Task {
    /// How tasks are shown everywhere: `api#12`.
    pub fn key(&self) -> String {
        format!("{}#{}", self.project, self.id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// Fields of a task to change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
}

pub struct Store {
    conn: Connection,
    statuses: Vec<String>,
    /// Held while the connection is open; declared after `conn` so it is released after it closes.
    _lock: Option<std::fs::File>,
}

impl Store {
    /// Opens the database. `lock` is kept until the store is dropped (see `Home::store`).
    pub fn open(path: &Path, statuses: Vec<String>, lock: Option<std::fs::File>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // The board and any number of agents share this file; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Per connection, not stored in the file. Also turns a WAL database from an
        // older kanzo back into a rollback-journal one once nothing else has it open.
        // `synchronous` stays at SQLite's default FULL, which keeps the file intact
        // when a board loses power.
        conn.pragma_update_and_check(None, "journal_mode", "TRUNCATE", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn, statuses, _lock: lock })
    }

    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    fn first_status(&self) -> &str {
        &self.statuses[0]
    }

    fn done_status(&self) -> &str {
        self.statuses.last().expect("config has at least two statuses")
    }

    /// Runs `f` in an immediate transaction, so concurrent writers queue up
    /// on the busy timeout instead of failing halfway.
    fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Brings the schema up to date. Writes nothing when it already is, so starting
    /// `kanzo mcp` for every agent session costs no disk write.
    pub fn migrate(&self) -> Result<()> {
        let current = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get::<_, String>(0))
            .optional()
            .unwrap_or(None);
        if current == Some(MIGRATIONS.len().to_string()) {
            return Ok(());
        }
        self.write(|c| {
            c.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
            let version: usize = c
                .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get::<_, String>(0))
                .optional()?
                .map_or(0, |v| v.parse().unwrap_or(0));
            if version > MIGRATIONS.len() {
                return Err(Error::Invalid(format!(
                    "kanzo.db has schema version {version}, newer than this kanzo supports ({}); update kanzo",
                    MIGRATIONS.len()
                )));
            }
            for sql in &MIGRATIONS[version..] {
                c.execute_batch(sql)?;
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [MIGRATIONS.len().to_string()],
            )?;
            Ok(())
        })
    }

    /// Bumped by triggers on every write; the board polls it.
    pub fn changes(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'changes'")?
            .query_row([], |r| r.get(0))?)
    }

    /// The current time, in the same format as the stored timestamps.
    pub fn now(&self) -> Result<String> {
        Ok(self.conn.query_row(&format!("SELECT {NOW}"), [], |r| r.get(0))?)
    }

    /// Makes `kanzo.db` complete on its own: rolls back a write that a crashed process
    /// left half done, and folds in a WAL left by an older kanzo.
    pub fn settle(&self) -> Result<()> {
        self.conn.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    // Projects

    const PROJECT_SELECT: &str = "SELECT p.id, p.slug, p.name, p.repo, p.created_at,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.status <> ?1),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id)
        FROM projects p";

    fn project_row(r: &Row) -> rusqlite::Result<Project> {
        Ok(Project {
            id: r.get(0)?,
            slug: r.get(1)?,
            name: r.get(2)?,
            repo: r.get(3)?,
            created_at: r.get(4)?,
            open: r.get(5)?,
            total: r.get(6)?,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let sql = format!("{} ORDER BY p.slug", Self::PROJECT_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([self.done_status()], Self::project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn project_where(&self, clause: &str, value: &str) -> Result<Option<Project>> {
        let sql = format!("{} WHERE {clause}", Self::PROJECT_SELECT);
        Ok(self.conn.query_row(&sql, params![self.done_status(), value], Self::project_row).optional()?)
    }

    pub fn project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        self.project_where("p.slug = ?2", &slug.trim().to_ascii_lowercase())
    }

    pub fn project_by_repo(&self, repo: &str) -> Result<Option<Project>> {
        match repo_from_url(repo) {
            Some(repo) => self.project_where("p.repo = ?2", &repo),
            None => Ok(None),
        }
    }

    /// Finds a project by slug, or by linked repo when given `owner/repo`.
    pub fn find_project(&self, name: &str) -> Result<Project> {
        let found = match self.project_by_slug(name)? {
            Some(p) => Some(p),
            None if name.contains('/') => self.project_by_repo(name)?,
            None => None,
        };
        found.ok_or_else(|| {
            let known = self.list_projects().unwrap_or_default();
            let names: Vec<_> = known.iter().map(|p| p.slug.as_str()).collect();
            let hint = if names.is_empty() {
                "there are no projects yet".to_string()
            } else {
                format!("projects: {}", names.join(", "))
            };
            Error::NotFound(format!("no project {name:?} ({hint})"))
        })
    }

    pub fn create_project(&self, slug: &str, name: &str, repo: Option<&str>) -> Result<Project> {
        let slug = slug.trim().to_ascii_lowercase();
        if !valid_slug(&slug) {
            return Err(Error::Invalid(format!(
                "project slug {slug:?} must be 1-32 lowercase letters, digits or -, starting with a letter or digit"
            )));
        }
        let name = match name.trim() {
            "" => slug.clone(),
            n => n.to_string(),
        };
        let repo = match repo.map(str::trim).filter(|r| !r.is_empty()) {
            None => None,
            Some(r) => Some(
                repo_from_url(r).ok_or_else(|| Error::Invalid(format!("repo {r:?} should look like owner/repo")))?,
            ),
        };
        if self.project_by_slug(&slug)?.is_some() {
            return Err(Error::Invalid(format!("project {slug} already exists")));
        }
        if let Some(r) = &repo
            && let Some(other) = self.project_by_repo(r)?
        {
            return Err(Error::Invalid(format!("repo {r} is already linked to project {}", other.slug)));
        }
        self.conn.execute("INSERT INTO projects (slug, name, repo) VALUES (?1, ?2, ?3)", params![slug, name, repo])?;
        self.find_project(&slug)
    }

    // Tasks

    const TASK_SELECT: &str = "SELECT t.id, p.slug, t.title, t.description, t.status, t.position,
            t.created_at, t.updated_at, (SELECT count(*) FROM comments c WHERE c.task_id = t.id)
        FROM tasks t JOIN projects p ON p.id = t.project_id";

    fn task_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            status: r.get(4)?,
            position: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            comments: r.get(8)?,
        })
    }

    /// Tasks in board order: by column as configured, then by position.
    /// Tasks whose status is no longer in the config sort last.
    pub fn list_tasks(&self, project_id: Option<i64>, status: Option<&str>) -> Result<Vec<Task>> {
        let sql = format!(
            "{} WHERE (?1 IS NULL OR t.project_id = ?1) AND (?2 IS NULL OR t.status = ?2) ORDER BY t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks: Vec<Task> =
            stmt.query_map(params![project_id, status], Self::task_row)?.collect::<rusqlite::Result<_>>()?;
        let column = |t: &Task| self.statuses.iter().position(|s| *s == t.status).unwrap_or(usize::MAX);
        tasks.sort_by_key(column);
        Ok(tasks)
    }

    pub fn get_task(&self, id: i64) -> Result<Task> {
        let sql = format!("{} WHERE t.id = ?1", Self::TASK_SELECT);
        self.conn
            .prepare_cached(&sql)?
            .query_row([id], Self::task_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no task #{id}")))
    }

    /// Looks up `api#12`, `#12` or `12`. A project prefix must match the task's project.
    pub fn find_task(&self, key: &str) -> Result<Task> {
        let (slug, id) = parse_task_key(key)?;
        let task = self.get_task(id)?;
        if let Some(slug) = slug
            && slug != task.project
        {
            return Err(Error::NotFound(format!("no task {key} (#{id} is {})", task.key())));
        }
        Ok(task)
    }

    fn check_status(&self, status: &str) -> Result<()> {
        if self.statuses.iter().any(|s| s == status) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("unknown status {status:?}; use one of: {}", self.statuses.join(", "))))
        }
    }

    fn end_of_column(c: &Connection, status: &str, except: i64) -> Result<f64> {
        let max: Option<f64> = c.query_row(
            "SELECT max(position) FROM tasks WHERE status = ?1 AND id <> ?2",
            params![status, except],
            |r| r.get(0),
        )?;
        Ok(max.map_or(1.0, |m| m.floor() + 1.0))
    }

    pub fn create_task(&self, project_id: i64, title: &str, description: &str) -> Result<Task> {
        let title = clean_title(title)?;
        let status = self.first_status();
        let id = self.write(|c| {
            let position = Self::end_of_column(c, status, 0)?;
            c.execute(
                "INSERT INTO tasks (project_id, title, description, status, position) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project_id, title, description.trim(), status, position],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        self.get_task(id)
    }

    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<Task> {
        let title = patch.title.as_deref().map(clean_title).transpose()?;
        if let Some(s) = &patch.status {
            self.check_status(s)?;
        }
        self.write(|c| {
            let current = self.get_task(id)?;
            if let Some(title) = title {
                c.execute(
                    &format!("UPDATE tasks SET title = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![title, id],
                )?;
            }
            if let Some(d) = &patch.description {
                c.execute(
                    &format!("UPDATE tasks SET description = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![d.trim(), id],
                )?;
            }
            if let Some(s) = &patch.status
                && *s != current.status
            {
                let position = Self::end_of_column(c, s, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![s, position, id],
                )?;
            }
            Ok(())
        })?;
        self.get_task(id)
    }

    /// Moves a task to `status`, just above task `before`, or to the bottom when `before` is `None`.
    pub fn move_task(&self, id: i64, status: &str, before: Option<i64>) -> Result<Task> {
        self.check_status(status)?;
        self.write(|c| {
            let current = self.get_task(id)?;
            let before = before.filter(|b| *b != id);
            let anchor: Option<f64> = match before {
                Some(b) => c
                    .query_row("SELECT position FROM tasks WHERE id = ?1 AND status = ?2", params![b, status], |r| {
                        r.get(0)
                    })
                    .optional()?,
                None => None,
            };
            let position = match anchor {
                None => Self::end_of_column(c, status, id)?,
                Some(mut next) => {
                    let before = before.expect("anchor comes from before");
                    let prev_of = |next: f64| -> Result<Option<f64>> {
                        Ok(c.query_row(
                            "SELECT max(position) FROM tasks WHERE status = ?1 AND position < ?2 AND id <> ?3",
                            params![status, next, id],
                            |r| r.get(0),
                        )?)
                    };
                    let mut prev = prev_of(next)?;
                    if prev.is_some_and(|p| next - p < 1e-9) {
                        // Out of room between two neighbours: renumber the column 1, 2, 3...
                        Self::renumber(c, status)?;
                        next = c.query_row("SELECT position FROM tasks WHERE id = ?1", [before], |r| r.get(0))?;
                        prev = prev_of(next)?;
                    }
                    match prev {
                        Some(p) => (p + next) / 2.0,
                        None => next - 1.0,
                    }
                }
            };
            let touched = if current.status != status { format!(", updated_at = {NOW}") } else { String::new() };
            c.execute(
                &format!("UPDATE tasks SET status = ?1, position = ?2{touched} WHERE id = ?3"),
                params![status, position, id],
            )?;
            Ok(())
        })?;
        self.get_task(id)
    }

    fn renumber(c: &Connection, status: &str) -> Result<()> {
        let ids: Vec<i64> = c
            .prepare("SELECT id FROM tasks WHERE status = ?1 ORDER BY position")?
            .query_map([status], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for (i, id) in ids.iter().enumerate() {
            c.execute("UPDATE tasks SET position = ?1 WHERE id = ?2", params![(i + 1) as f64, id])?;
        }
        Ok(())
    }

    /// Takes the top task of the first column and moves it to the bottom of the second.
    pub fn next_task(&self, project_id: Option<i64>) -> Result<Option<Task>> {
        let from = self.first_status();
        let to = &self.statuses[1];
        let id = self.write(|c| {
            let id: Option<i64> = c
                .query_row(
                    "SELECT id FROM tasks WHERE status = ?1 AND (?2 IS NULL OR project_id = ?2) ORDER BY position LIMIT 1",
                    params![from, project_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                let position = Self::end_of_column(c, to, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![to, position, id],
                )?;
            }
            Ok(id)
        })?;
        id.map(|id| self.get_task(id)).transpose()
    }

    /// Deletes a task and its comments; returns the task as it was.
    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let task = self.get_task(id)?;
        self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(task)
    }

    // Comments

    pub fn list_comments(&self, task_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, task_id, author, body, created_at FROM comments WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([task_id], |r| {
            Ok(Comment { id: r.get(0)?, task_id: r.get(1)?, author: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_comment(&self, task_id: i64, author: &str, body: &str) -> Result<Comment> {
        let body = body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        let author = match author.trim() {
            "" => "me",
            a => a,
        };
        self.get_task(task_id)?;
        self.conn.execute(
            "INSERT INTO comments (task_id, author, body) VALUES (?1, ?2, ?3)",
            params![task_id, author, body],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.conn.query_row(
            "SELECT id, task_id, author, body, created_at FROM comments WHERE id = ?1",
            [id],
            |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    author: r.get(2)?,
                    body: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )?)
    }
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn clean_title(title: &str) -> Result<String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err(Error::Invalid("task title is empty".into()));
    }
    Ok(title)
}

/// Splits `api#12`, `#12` or `12` into an optional project slug and the task id.
pub fn parse_task_key(key: &str) -> Result<(Option<String>, i64)> {
    let key = key.trim();
    let (slug, num) = match key.rsplit_once('#') {
        Some((s, n)) => (Some(s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()), n),
        None => (None, key),
    };
    let id = num
        .trim()
        .parse::<i64>()
        .map_err(|_| Error::Invalid(format!("{key:?} is not a task id; use a form like api#12 or 12")))?;
    Ok((slug, id))
}

/// Normalises a GitHub repo given as `owner/repo` or as any git remote URL
/// (`https://github.com/owner/repo.git`, `git@github.com:owner/repo`, ...) to lowercase `owner/repo`.
pub fn repo_from_url(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let path = if let Some((_, rest)) = s.split_once("://") {
        // scheme://[user@]host[:port]/owner/repo
        rest.split_once('/')?.1
    } else if let Some((_, rest)) = s.split_once(':') {
        // user@host:owner/repo
        rest
    } else {
        s
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || (path == s && parts.len() != 2) {
        return None;
    }
    let (owner, repo) = (parts[parts.len() - 2], parts[parts.len() - 1]);
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| format!("{owner}/{repo}").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let statuses = ["todo", "doing", "review", "done"].map(String::from).to_vec();
        let store = Store::open(&dir.path().join("kanzo.db"), statuses, None).unwrap();
        store.migrate().unwrap();
        (dir, store)
    }

    fn titles(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn migrate_is_idempotent() {
        let (_dir, s) = store();
        s.migrate().unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len().to_string());
    }

    #[test]
    fn projects() {
        let (_dir, s) = store();
        let p = s.create_project("API", "API server", Some("https://github.com/Me/Api.git")).unwrap();
        assert_eq!((p.slug.as_str(), p.repo.as_deref()), ("api", Some("me/api")));
        assert_eq!(s.find_project("me/api").unwrap().id, p.id);
        assert_eq!(s.project_by_repo("git@github.com:me/api.git").unwrap().unwrap().id, p.id);
        assert!(s.create_project("api", "", None).unwrap_err().to_string().contains("already exists"));
        assert!(s.create_project("web", "", Some("me/api")).unwrap_err().to_string().contains("already linked"));
        assert!(s.create_project("bad slug", "", None).is_err());
        let web = s.create_project("web", "", None).unwrap();
        assert_eq!(web.name, "web");
        assert!(s.find_project("nope").unwrap_err().to_string().contains("api, web"));
    }

    #[test]
    fn task_lifecycle() {
        let (_dir, s) = store();
        let p = s.create_project("api", "API", None).unwrap();
        let t = s.create_task(p.id, "  Fix   login ", "Steps:\n1. log in\n").unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str(), t.key()), ("Fix login", "todo", format!("api#{}", t.id)));
        assert_eq!(s.find_task(&t.key()).unwrap().id, t.id);
        assert!(s.find_task(&format!("web#{}", t.id)).is_err());

        let t = s
            .update_task(
                t.id,
                TaskPatch { status: Some("review".into()), title: Some("Fix login bug".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("Fix login bug", "review"));
        assert!(s.update_task(t.id, TaskPatch { status: Some("nope".into()), ..Default::default() }).is_err());
        assert!(s.update_task(t.id, TaskPatch { title: Some("  ".into()), ..Default::default() }).is_err());

        s.add_comment(t.id, "claude-code", "Done, see PR").unwrap();
        s.add_comment(t.id, "", "thanks").unwrap();
        let comments = s.list_comments(t.id).unwrap();
        assert_eq!(comments.iter().map(|c| c.author.as_str()).collect::<Vec<_>>(), ["claude-code", "me"]);
        assert!(s.add_comment(t.id, "me", " ").is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        assert_eq!(s.find_project("api").unwrap().open, 1);

        s.delete_task(t.id).unwrap();
        assert!(s.get_task(t.id).is_err());
        let orphans: i64 = s.conn.query_row("SELECT count(*) FROM comments", [], |r| r.get(0)).unwrap();
        assert_eq!(orphans, 0);
        // Ids are never reused, so api#12 always means the same task.
        let t2 = s.create_task(p.id, "Another", "").unwrap();
        assert!(t2.id > t.id);
    }

    #[test]
    fn ordering_and_moves() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        let c = s.create_task(p.id, "c", "").unwrap();
        assert_eq!(titles(&s.list_tasks(Some(p.id), None).unwrap()), ["a", "b", "c"]);

        s.move_task(c.id, "todo", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["c", "a", "b"]);
        s.move_task(c.id, "todo", None).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["a", "b", "c"]);
        s.move_task(a.id, "doing", None).unwrap();
        s.move_task(b.id, "doing", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["c", "b", "a"]);

        // Many inserts at the same spot run out of float room and trigger a renumber.
        let mut moving = [b.id, c.id];
        for _ in 0..80 {
            s.move_task(moving[0], "doing", Some(a.id)).unwrap();
            moving.swap(0, 1);
        }
        assert_eq!(titles(&s.list_tasks(None, Some("doing")).unwrap()).last(), Some(&"a"));
        assert!(s.move_task(a.id, "nope", None).is_err());
    }

    #[test]
    fn next_task_takes_top_todo() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "web first", "").unwrap();
        s.create_task(api.id, "api first", "").unwrap();
        s.create_task(api.id, "api second", "").unwrap();

        let t = s.next_task(Some(api.id)).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("api first", "doing"));
        assert_eq!(s.next_task(None).unwrap().unwrap().title, "web first");
        assert_eq!(s.next_task(Some(api.id)).unwrap().unwrap().title, "api second");
        assert!(s.next_task(Some(api.id)).unwrap().is_none());
    }

    #[test]
    fn change_counter_moves_on_every_write() {
        let (_dir, s) = store();
        let c0 = s.changes().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let c1 = s.changes().unwrap();
        let t = s.create_task(p.id, "x", "").unwrap();
        let c2 = s.changes().unwrap();
        s.add_comment(t.id, "me", "hi").unwrap();
        let c3 = s.changes().unwrap();
        assert!(c0 < c1 && c1 < c2 && c2 < c3);
    }

    #[test]
    fn task_keys() {
        assert_eq!(parse_task_key("api#12").unwrap(), (Some("api".into()), 12));
        assert_eq!(parse_task_key("#12").unwrap(), (None, 12));
        assert_eq!(parse_task_key(" 12 ").unwrap(), (None, 12));
        assert!(parse_task_key("api#x").is_err());
    }

    #[test]
    fn repos() {
        for url in [
            "owner/repo",
            "Owner/Repo",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "git@github-work:owner/repo.git",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("owner/repo"), "{url}");
        }
        assert_eq!(repo_from_url("repo"), None);
        assert_eq!(repo_from_url("a/b/c"), None);
        assert_eq!(repo_from_url("own er/repo"), None);
    }
}
