//! All reads and writes of `brokan.db`. Plain SQL, one short-lived connection per operation.
//!
//! The database uses a rollback journal (`journal_mode = TRUNCATE`), not WAL. With a
//! connection per operation, WAL would set up and tear down its `-wal` and `-shm` files
//! on every request; the rollback journal makes a read touch no extra files at all and
//! measured about 3x faster per operation. Writes queue on the busy timeout.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Deserializer, Serialize};

/// The oldest schema version whose Brokan can still use a database at the latest one.
/// Adding a table or a column with a default generally leaves older versions working.
/// Archived projects must be hidden from agents, though; v9 and older do not filter them.
/// Refuse those clients rather than allowing them to claim archived tasks from synced boards.
const COMPATIBLE_FROM: usize = 10;

/// Schema migrations, applied in order. Index + 1 is the `schema_version` they lead to.
const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    // 2: a colour picked for the project; NULL leaves the board to choose one.
    "ALTER TABLE projects ADD COLUMN color TEXT;",
    // 3: a task's tags, comma-separated (see `clean_tags`).
    "ALTER TABLE tasks ADD COLUMN tags TEXT NOT NULL DEFAULT '';",
    // 4: any number of repos per project. Each repo belongs to one project. The old
    // projects.repo column stays, empty: SQLite can only drop it by rebuilding the table.
    "CREATE TABLE project_repos (
         repo       TEXT PRIMARY KEY,
         project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE
     );
     CREATE INDEX project_repos_by_project ON project_repos(project_id);
     INSERT INTO project_repos (repo, project_id) SELECT repo, id FROM projects WHERE repo IS NOT NULL;
     UPDATE projects SET repo = NULL;
     CREATE TRIGGER project_repos_insert AFTER INSERT ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER project_repos_delete AFTER DELETE ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 5: images attached to a task, or to one of its comments; the files are in images/.
    "CREATE TABLE images (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         comment_id INTEGER REFERENCES comments(id) ON DELETE CASCADE,
         file       TEXT NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE INDEX images_by_task ON images(task_id);
     CREATE TRIGGER images_insert AFTER INSERT ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER images_delete AFTER DELETE ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 6: tasks for a person, which agents leave alone.
    "ALTER TABLE tasks ADD COLUMN human INTEGER NOT NULL DEFAULT 0;",
    // 7: optional priority; a default keeps existing boards and older Brokan compatible.
    "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'none'
        CHECK (priority IN ('none', 'low', 'medium', 'high', 'urgent'));",
    // 8: optional project icon; older boards and older Brokan still show a folder.
    "ALTER TABLE projects ADD COLUMN icon TEXT
        CHECK (icon IS NULL OR icon IN
            ('folder', 'code', 'terminal', 'globe', 'database', 'server',
             'rocket', 'book', 'briefcase', 'bug', 'palette', 'zap'));",
    // 9: time entries are additive, so existing boards and older clients keep working.
    // Only one timer can run at a time across the board.
    "CREATE TABLE time_entries (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         started_at INTEGER NOT NULL,
         ended_at   INTEGER,
         seconds    INTEGER NOT NULL DEFAULT 0 CHECK (seconds >= 0),
         CHECK ((ended_at IS NULL AND seconds = 0) OR (ended_at IS NOT NULL AND seconds > 0))
     );
     CREATE INDEX time_entries_by_task ON time_entries(task_id, started_at);
     CREATE UNIQUE INDEX time_entries_one_running ON time_entries((1)) WHERE ended_at IS NULL;
     CREATE TRIGGER time_entries_insert AFTER INSERT ON time_entries BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER time_entries_update AFTER UPDATE ON time_entries BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER time_entries_delete AFTER DELETE ON time_entries BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 10: archive projects without deleting their tasks, repos or time entries.
    "ALTER TABLE projects ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));",
    // 11: an optional UTC deadline; existing tasks remain unscheduled.
    "ALTER TABLE tasks ADD COLUMN deadline_at TEXT;",
];

/// The colours and icons a project can be given; the board maps each name to a shade or SVG.
const PROJECT_COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];
const PROJECT_ICONS: &[&str] = &[
    "folder",
    "code",
    "terminal",
    "globe",
    "database",
    "server",
    "rocket",
    "book",
    "briefcase",
    "bug",
    "palette",
    "zap",
];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    /// Another process holds the data folder for longer than we wait, like a long `brokan sync`.
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
    /// Linked GitHub repos as `owner/repo`, in the order they were linked.
    pub repos: Vec<String>,
    /// One of `PROJECT_COLORS`, or `None` when never picked.
    pub color: Option<String>,
    /// One of `PROJECT_ICONS`, or `None` for the default folder.
    pub icon: Option<String>,
    pub archived: bool,
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
    /// Lowercase, without duplicates, in the order they were given.
    pub tags: Vec<String>,
    /// Work for a person: agents leave it alone, and `next_task` never picks it.
    pub human: bool,
    pub priority: String,
    /// UTC instant, YYYY-MM-DDTHH:MM:SSZ; None means no deadline.
    pub deadline_at: Option<String>,
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

/// An image attached to a task, or to one of its comments.
#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub id: i64,
    pub task_id: i64,
    pub comment_id: Option<i64>,
    /// Its file name in the data folder's `images/`, like `6fec521a01297ab5.png`.
    pub file: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeEntry {
    pub id: i64,
    pub task_id: i64,
    /// Unix seconds (UTC). A manual entry has equal start and end times.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Completed time; zero only while the timer is running.
    pub seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct TimeReport {
    pub entries: Vec<TimeEntry>,
    /// Includes the elapsed time of a running timer as of the response.
    pub total_seconds: i64,
    pub running_since: Option<i64>,
    /// Snapshot time, so the client can display the running total between requests.
    pub as_of: i64,
}

#[derive(Debug, Serialize)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct ProjectTime {
    pub slug: String,
    pub name: String,
    pub color: Option<String>,
    pub seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct DayTime {
    pub day: String,
    pub seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub statuses: Vec<StatusCount>,
    pub projects: Vec<ProjectTime>,
    pub days: Vec<DayTime>,
    /// All-time total, including active timers as of the response.
    pub total_seconds: i64,
    pub running_seconds: i64,
    pub tasks: i64,
}

/// Fields of a task to change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    /// Moves the task to this project (by id).
    pub project_id: Option<i64>,
    /// Comma-separated; replaces all the tags, and an empty string clears them.
    pub tags: Option<String>,
    pub human: Option<bool>,
    pub priority: Option<String>,
    /// None leaves the deadline unchanged; Some(None) clears it.
    pub deadline_at: Option<Option<String>>,
}

/// On JSON patches, distinguish an omitted field from an explicit null (clear).
pub fn nullable_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

/// The board's columns, left to right. New tasks go in todo, agents take theirs from
/// there and move them to doing, and tasks in done count as done.
const STATUSES: [&str; 4] = ["todo", "doing", "review", "done"];
const PRIORITIES: [&str; 5] = ["none", "low", "medium", "high", "urgent"];

fn check_priority(priority: &str) -> Result<&str> {
    if PRIORITIES.contains(&priority) {
        Ok(priority)
    } else {
        Err(Error::Invalid(format!("unknown priority {priority:?}; use one of: {}", PRIORITIES.join(", "))))
    }
}

/// Accept only canonical UTC instants. Validating before a transaction avoids partial task writes.
fn check_deadline(deadline: &str) -> Result<&str> {
    let b = deadline.as_bytes();
    let invalid = || Error::Invalid("deadline_at must be a valid UTC date/time (YYYY-MM-DDTHH:MM:SSZ)".into());
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
        || ![0..4, 5..7, 8..10, 11..13, 14..16, 17..19].iter().all(|r| b[r.clone()].iter().all(u8::is_ascii_digit))
    {
        return Err(invalid());
    }
    let number =
        |r: std::ops::Range<usize>| -> u32 { b[r].iter().fold(0, |n, digit| n * 10 + u32::from(digit - b'0')) };
    let year = number(0..4);
    let month = number(5..7);
    let day = number(8..10);
    let hour = number(11..13);
    let minute = number(14..16);
    let second = number(17..19);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    if year == 0 || day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err(invalid());
    }
    Ok(deadline)
}

/// An optional column before todo, for tasks that agents leave alone.
pub const BACKLOG: &str = "backlog";

pub struct Store {
    conn: Connection,
    statuses: Vec<String>,
    /// Held while the connection is open; declared after `conn` so it is released after it closes.
    _lock: Option<std::fs::File>,
}

impl Store {
    /// Opens the database. `lock` is kept until the store is dropped (see `Home::store`).
    pub fn open(path: &Path, lock: Option<std::fs::File>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // The board and any number of agents share this file; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Per connection, not stored in the file. Also turns a WAL database from an
        // older brokan back into a rollback-journal one once nothing else has it open.
        // `synchronous` stays at SQLite's default FULL, which keeps the file intact
        // when a board loses power.
        conn.pragma_update_and_check(None, "journal_mode", "TRUNCATE", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Before the first migration there is no meta table, and so no backlog.
        let backlog = conn
            .query_row("SELECT value FROM meta WHERE key = 'backlog'", [], |r| r.get::<_, String>(0))
            .is_ok_and(|v| v == "1");
        let statuses = backlog.then_some(BACKLOG).into_iter().chain(STATUSES).map(String::from).collect();
        Ok(Self { conn, statuses, _lock: lock })
    }

    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    fn done_status(&self) -> &str {
        STATUSES[3]
    }

    /// Shows or hides the backlog column. It must be empty to hide it, or its
    /// tasks would vanish from the board.
    pub fn set_backlog(&self, on: bool) -> Result<()> {
        self.write(|c| {
            let left: i64 = c.query_row("SELECT count(*) FROM tasks WHERE status = ?1", [BACKLOG], |r| r.get(0))?;
            if !on && left > 0 {
                let tasks = if left == 1 { "task" } else { "tasks" };
                return Err(Error::Invalid(format!("move the {left} {tasks} out of backlog first")));
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('backlog', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [if on { "1" } else { "0" }],
            )?;
            // Other open boards redraw their columns.
            c.execute("UPDATE meta SET value = value + 1 WHERE key = 'changes'", [])?;
            Ok(())
        })
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
    /// `brokan mcp` for every agent session costs no disk write.
    pub fn migrate(&self) -> Result<()> {
        let meta = |c: &Connection, key: &str| -> rusqlite::Result<Option<usize>> {
            Ok(c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get::<_, String>(0))
                .optional()?
                .and_then(|v| v.parse().ok()))
        };
        let latest = MIGRATIONS.len();
        let current =
            (meta(&self.conn, "schema_version").unwrap_or(None), meta(&self.conn, "compatible").unwrap_or(None));
        if current == (Some(latest), Some(COMPATIBLE_FROM)) {
            return Ok(());
        }
        self.write(|c| {
            c.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
            let version = meta(c, "schema_version")?.unwrap_or(0);
            if version > latest {
                // From a newer Brokan, maybe synced from another machine: use it as it is
                // when this version can, without touching its schema.
                if meta(c, "compatible")?.is_some_and(|oldest| oldest <= latest) {
                    return Ok(());
                }
                return Err(Error::Invalid(
                    "this board was saved by a newer version of Brokan; update this one with `brokan update`".into(),
                ));
            }
            for sql in &MIGRATIONS[version..] {
                c.execute_batch(sql)?;
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('compatible', ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [latest.to_string(), COMPATIBLE_FROM.to_string()],
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

    /// Makes `brokan.db` complete on its own: rolls back a write that a crashed process
    /// left half done, and folds in a WAL left by an older brokan.
    pub fn settle(&self) -> Result<()> {
        self.conn.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    // Projects

    const PROJECT_SELECT: &str = "SELECT p.id, p.slug, p.name,
            (SELECT group_concat(repo, ',' ORDER BY r.rowid) FROM project_repos r WHERE r.project_id = p.id),
            p.color, p.created_at,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.status <> ?1),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id), p.icon, p.archived
        FROM projects p";

    fn project_row(r: &Row) -> rusqlite::Result<Project> {
        Ok(Project {
            id: r.get(0)?,
            slug: r.get(1)?,
            name: r.get(2)?,
            repos: r.get::<_, Option<String>>(3)?.map(|s| s.split(',').map(String::from).collect()).unwrap_or_default(),
            color: r.get(4)?,
            created_at: r.get(5)?,
            open: r.get(6)?,
            total: r.get(7)?,
            icon: r.get(8)?,
            archived: r.get(9)?,
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
            Some(repo) => self.project_where("p.id = (SELECT project_id FROM project_repos WHERE repo = ?2)", &repo),
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
        let repo = repo.map(str::trim).filter(|r| !r.is_empty()).map(clean_repo).transpose()?;
        if self.project_by_slug(&slug)?.is_some() {
            return Err(Error::Invalid(format!("project {slug} already exists")));
        }
        if let Some(r) = &repo {
            self.check_repo_free(r)?;
        }
        let created = self.write(|c| {
            c.execute("INSERT INTO projects (slug, name) VALUES (?1, ?2)", params![slug, name])?;
            if let Some(r) = &repo {
                c.execute(
                    "INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)",
                    params![r, c.last_insert_rowid()],
                )?;
            }
            Ok(())
        });
        match created {
            // Someone created the same project or linked the repo between our checks and the insert.
            Err(Error::Db(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(Error::Invalid(format!("project {slug} or its repo already exists")))
            }
            other => other,
        }?;
        self.find_project(&slug)
    }

    /// Links another repo to a project. Each repo belongs to one project.
    pub fn link_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = clean_repo(repo)?;
        if project.repos.contains(&repo) {
            return Ok(project);
        }
        self.check_repo_free(&repo)?;
        match self
            .conn
            .execute("INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)", params![repo, project.id])
        {
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                return Err(Error::Invalid(format!("repo {repo} is already linked to another project")));
            }
            other => other?,
        };
        self.find_project(&project.slug)
    }

    pub fn unlink_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = repo_from_url(repo).unwrap_or_else(|| repo.trim().to_ascii_lowercase());
        let removed = self
            .conn
            .execute("DELETE FROM project_repos WHERE repo = ?1 AND project_id = ?2", params![repo, project.id])?;
        if removed == 0 {
            return Err(Error::NotFound(format!("repo {repo} is not linked to project {}", project.slug)));
        }
        self.find_project(&project.slug)
    }

    fn check_repo_free(&self, repo: &str) -> Result<()> {
        match self.project_by_repo(repo)? {
            Some(other) => Err(Error::Invalid(format!("repo {repo} is already linked to project {}", other.slug))),
            None => Ok(()),
        }
    }

    /// Changes project appearance and name together, without partial writes on bad input.
    /// Its slug, and so its task keys like api#12, stay.
    pub fn update_project(
        &self,
        slug: &str,
        name: Option<&str>,
        color: Option<&str>,
        icon: Option<&str>,
    ) -> Result<Project> {
        let name = name.map(squash);
        if name.as_deref() == Some("") {
            return Err(Error::Invalid("project name is empty".into()));
        }
        if let Some(color) = color.filter(|c| !PROJECT_COLORS.contains(c)) {
            return Err(Error::Invalid(format!("unknown color {color:?}; use one of: {}", PROJECT_COLORS.join(", "))));
        }
        if let Some(icon) = icon.filter(|i| !PROJECT_ICONS.contains(i)) {
            return Err(Error::Invalid(format!("unknown icon {icon:?}; use one of: {}", PROJECT_ICONS.join(", "))));
        }
        let project = self.find_project(slug)?;
        if name.is_none() && color.is_none() && icon.is_none() {
            return Ok(project);
        }
        self.conn.execute(
            "UPDATE projects SET name = COALESCE(?1, name), color = COALESCE(?2, color), icon = COALESCE(?3, icon)
             WHERE id = ?4",
            params![name, color, icon, project.id],
        )?;
        self.find_project(&project.slug)
    }

    /// Archives or restores a project without changing its tasks or linked repos.
    pub fn set_project_archived(&self, slug: &str, archived: bool) -> Result<Project> {
        let project = self.find_project(slug)?;
        self.conn.execute(
            "UPDATE projects SET archived = ?1 WHERE id = ?2 AND archived <> ?1",
            params![archived, project.id],
        )?;
        self.find_project(&project.slug)
    }

    /// Deletes a project with all its tasks and their comments; returns the project as it was.
    pub fn delete_project(&self, slug: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        // Tasks and comments go with it through ON DELETE CASCADE.
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [project.id])?;
        Ok(project)
    }

    // Tasks

    const TASK_SELECT: &str = "SELECT t.id, p.slug, t.title, t.description, t.status, t.position,
            t.created_at, t.updated_at, (SELECT count(*) FROM comments c WHERE c.task_id = t.id), t.tags, t.human, t.priority, t.deadline_at
        FROM tasks t JOIN projects p ON p.id = t.project_id";

    fn task_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            tags: r.get::<_, String>(9)?.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
            human: r.get(10)?,
            priority: r.get(11)?,
            deadline_at: r.get(12)?,
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
        Ok(self.list_tasks_keeping_done(project_id, status, None)?.0)
    }

    /// `list_tasks`, but of the done column, which only grows, just the newest `keep_done`
    /// tasks, read from the database without the rest; also says how many were left out.
    pub fn list_tasks_keeping_done(
        &self,
        project_id: Option<i64>,
        status: Option<&str>,
        keep_done: Option<usize>,
    ) -> Result<(Vec<Task>, usize)> {
        let done = self.done_status();
        // SQLite reads a negative LIMIT as no limit.
        let limit = keep_done.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE (?1 IS NULL OR t.project_id = ?1) AND (?1 IS NOT NULL OR p.archived = 0)
               AND (?2 IS NULL OR t.status = ?2)
               AND (t.status <> ?3 OR t.id IN (SELECT recent.id FROM tasks recent
                    JOIN projects owner ON owner.id = recent.project_id WHERE recent.status = ?3
                    AND (?1 IS NULL OR recent.project_id = ?1)
                    AND (?1 IS NOT NULL OR owner.archived = 0)
                    ORDER BY recent.position DESC LIMIT ?4))
             ORDER BY t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks: Vec<Task> = stmt
            .query_map(params![project_id, status, done, limit], Self::task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let column = |t: &Task| self.statuses().iter().position(|s| *s == t.status).unwrap_or(usize::MAX);
        tasks.sort_by_key(column);
        let hidden = match keep_done {
            Some(keep) if status.is_none_or(|s| s == done) => {
                let all: i64 = self.conn.query_row(
                    "SELECT count(*) FROM tasks t JOIN projects p ON p.id = t.project_id
                     WHERE t.status = ?1 AND (?2 IS NULL OR t.project_id = ?2)
                       AND (?2 IS NOT NULL OR p.archived = 0)",
                    params![done, project_id],
                    |r| r.get(0),
                )?;
                usize::try_from(all).unwrap_or(0).saturating_sub(keep)
            }
            _ => 0,
        };
        Ok((tasks, hidden))
    }

    /// Tasks of any project whose key, title, description, tags or project hold every
    /// word of `query`: open ones first, then the most recently changed. An empty query
    /// gives the most recently changed tasks.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Task>> {
        let words: Vec<String> = query.split_whitespace().take(10).map(|w| w.to_lowercase()).collect();
        let mut sql = format!("{} WHERE p.archived = 0", Self::TASK_SELECT);
        for i in 0..words.len() {
            sql.push_str(&format!(
                " AND lower(p.slug || '#' || t.id || ' ' || t.title || ' ' || t.description || ' ' || t.tags) \
                 LIKE ?{} ESCAPE '\\'",
                i + 2
            ));
        }
        sql.push_str(&format!(" ORDER BY t.status = ?1, t.updated_at DESC, t.id DESC LIMIT {limit}"));
        let mut values: Vec<String> = vec![self.done_status().to_string()];
        for w in &words {
            let escaped = w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            values.push(format!("%{escaped}%"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), Self::task_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        if self.statuses().iter().any(|s| s == status) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("unknown status {status:?}; use one of: {}", self.statuses().join(", "))))
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

    /// Adds a task at the bottom of todo; a shorthand for tests.
    #[cfg(test)]
    pub fn create_task(&self, project_id: i64, title: &str, description: &str) -> Result<Task> {
        self.create_task_in(project_id, None, title, description, None)
    }

    /// Adds a task at the bottom of `status`, todo when left out (test helper).
    #[cfg(test)]
    pub fn create_task_in(
        &self,
        project_id: i64,
        status: Option<&str>,
        title: &str,
        description: &str,
        priority: Option<&str>,
    ) -> Result<Task> {
        self.create_task_with_deadline(project_id, status, title, description, priority, None)
    }

    /// Create a task with an optional UTC deadline, validating it before any write.
    pub fn create_task_with_deadline(
        &self,
        project_id: i64,
        status: Option<&str>,
        title: &str,
        description: &str,
        priority: Option<&str>,
        deadline_at: Option<&str>,
    ) -> Result<Task> {
        let title = clean_title(title)?;
        let status = status.unwrap_or(STATUSES[0]);
        self.check_status(status)?;
        let priority = check_priority(priority.unwrap_or(PRIORITIES[0]))?;
        let deadline_at = deadline_at.map(check_deadline).transpose()?;
        self.ensure_active_project(project_id)?;
        let id = self.write(|c| {
            let position = Self::end_of_column(c, status, 0)?;
            c.execute(
                "INSERT INTO tasks (project_id, title, description, status, position, priority, deadline_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![project_id, title, description.trim(), status, position, priority, deadline_at],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        self.get_task(id)
    }

    fn ensure_active_project(&self, project_id: i64) -> Result<()> {
        let archived: Option<bool> = self
            .conn
            .query_row("SELECT archived FROM projects WHERE id = ?1", [project_id], |r| r.get(0))
            .optional()?;
        match archived {
            Some(false) => Ok(()),
            Some(true) => Err(Error::Invalid("restore the archived project before adding or moving tasks".into())),
            None => Err(Error::NotFound(format!("no project with id {project_id}"))),
        }
    }

    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<Task> {
        if let Some(project_id) = patch.project_id {
            self.ensure_active_project(project_id)?;
        }
        let title = patch.title.as_deref().map(clean_title).transpose()?;
        if let Some(s) = &patch.status {
            self.check_status(s)?;
        }
        if let Some(p) = &patch.priority {
            check_priority(p)?;
        }
        if let Some(Some(deadline)) = &patch.deadline_at {
            check_deadline(deadline)?;
        }
        self.write(|c| {
            let current = self.get_task(id)?;
            if let Some(title) = title {
                c.execute(
                    &format!("UPDATE tasks SET title = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![title, id],
                )?;
            }
            if let Some(p) = patch.project_id {
                c.execute(
                    &format!("UPDATE tasks SET project_id = ?1, updated_at = {NOW} WHERE id = ?2 AND project_id <> ?1"),
                    params![p, id],
                )?;
            }
            if let Some(human) = patch.human {
                c.execute(
                    &format!("UPDATE tasks SET human = ?1, updated_at = {NOW} WHERE id = ?2 AND human <> ?1"),
                    params![human, id],
                )?;
            }
            if let Some(priority) = &patch.priority {
                c.execute(
                    &format!("UPDATE tasks SET priority = ?1, updated_at = {NOW} WHERE id = ?2 AND priority <> ?1"),
                    params![priority, id],
                )?;
            }
            if let Some(deadline_at) = &patch.deadline_at {
                c.execute(
                    &format!(
                        "UPDATE tasks SET deadline_at = ?1, updated_at = {NOW} WHERE id = ?2 AND deadline_at IS NOT ?1"
                    ),
                    params![deadline_at, id],
                )?;
            }
            if let Some(t) = &patch.tags {
                c.execute(
                    &format!("UPDATE tasks SET tags = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![clean_tags(t).join(","), id],
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
            // The task to go above, with its position, if it's in that column.
            let anchor: Option<(i64, f64)> = match before {
                Some(b) => c
                    .query_row("SELECT position FROM tasks WHERE id = ?1 AND status = ?2", params![b, status], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .map(|p| (b, p)),
                None => None,
            };
            let position = match anchor {
                None => Self::end_of_column(c, status, id)?,
                Some((before, mut next)) => {
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
        let mut update = c.prepare("UPDATE tasks SET position = ?1 WHERE id = ?2")?;
        for (i, id) in ids.iter().enumerate() {
            update.execute(params![(i + 1) as f64, id])?;
        }
        Ok(())
    }

    /// Takes the highest-priority todo task that isn't for a person, breaking ties by
    /// board position, and moves it to the bottom of doing.
    pub fn next_task(&self, project_id: Option<i64>) -> Result<Option<Task>> {
        let [from, to, ..] = STATUSES;
        let id = self.write(|c| {
            let id: Option<i64> = c
                .query_row(
                    "SELECT t.id FROM tasks t JOIN projects p ON p.id = t.project_id
                     WHERE t.status = ?1 AND t.human = 0 AND p.archived = 0 AND (?2 IS NULL OR t.project_id = ?2) \
                     ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                         WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, t.position, t.id LIMIT 1",
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

    // Time tracking. All times are UTC Unix seconds; only completed entries store
    // seconds. The UI counts an active timer as of each response without writing
    // the database on every tick.

    fn unix_now(c: &Connection) -> Result<i64> {
        Ok(c.query_row("SELECT unixepoch()", [], |r| r.get(0))?)
    }

    pub fn time_report(&self, task_id: i64) -> Result<TimeReport> {
        self.get_task(task_id)?;
        let now = Self::unix_now(&self.conn)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, task_id, started_at, ended_at, seconds FROM time_entries
             WHERE task_id = ?1 ORDER BY started_at DESC, id DESC",
        )?;
        let entries: Vec<TimeEntry> = stmt
            .query_map([task_id], |r| {
                Ok(TimeEntry {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    seconds: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let total_seconds =
            entries.iter().map(|e| e.ended_at.map_or_else(|| (now - e.started_at).max(0), |_| e.seconds)).sum();
        let running_since = entries.iter().find(|e| e.ended_at.is_none()).map(|e| e.started_at);
        Ok(TimeReport { entries, total_seconds, running_since, as_of: now })
    }

    pub fn start_timer(&self, task_id: i64) -> Result<TimeReport> {
        self.write(|c| {
            self.get_task(task_id)?;
            let running: Option<i64> =
                c.query_row("SELECT task_id FROM time_entries WHERE ended_at IS NULL", [], |r| r.get(0)).optional()?;
            match running {
                Some(id) if id == task_id => Ok(()), // a repeated start is a no-op
                Some(id) => Err(Error::Invalid(format!("timer already running for task #{id}; stop it first"))),
                None => {
                    c.execute("INSERT INTO time_entries (task_id, started_at) VALUES (?1, unixepoch())", [task_id])?;
                    Ok(())
                }
            }
        })?;
        self.time_report(task_id)
    }

    pub fn stop_timer(&self, task_id: i64) -> Result<TimeReport> {
        self.write(|c| {
            self.get_task(task_id)?;
            let now = Self::unix_now(c)?;
            c.execute(
                "UPDATE time_entries SET ended_at = ?1, seconds = max(1, ?1 - started_at)
                 WHERE task_id = ?2 AND ended_at IS NULL",
                params![now, task_id],
            )?;
            Ok(()) // a repeated stop is a no-op
        })?;
        self.time_report(task_id)
    }

    /// Adds a manual entry in whole minutes (1 minute to 24 hours).
    pub fn log_time(&self, task_id: i64, minutes: i64) -> Result<TimeReport> {
        if !(1..=1440).contains(&minutes) {
            return Err(Error::Invalid("minutes must be between 1 and 1440".into()));
        }
        self.write(|c| {
            self.get_task(task_id)?;
            c.execute(
                "INSERT INTO time_entries (task_id, started_at, ended_at, seconds)
                 VALUES (?1, unixepoch(), unixepoch(), ?2)",
                params![task_id, minutes * 60],
            )?;
            Ok(())
        })?;
        self.time_report(task_id)
    }

    pub fn delete_time(&self, task_id: i64, entry_id: i64) -> Result<TimeReport> {
        self.write(|c| {
            self.get_task(task_id)?;
            let removed = c.execute(
                "DELETE FROM time_entries WHERE id = ?1 AND task_id = ?2 AND ended_at IS NOT NULL",
                params![entry_id, task_id],
            )?;
            if removed == 0 {
                return Err(Error::NotFound(format!("no completed time entry #{entry_id} for task #{task_id}")));
            }
            Ok(())
        })?;
        self.time_report(task_id)
    }

    /// Counts every task, even older done tasks omitted from the board response.
    /// The seven UTC days group each entry by when it began; manual entries use
    /// the day they were logged. Active elapsed time is included at query time.
    pub fn dashboard(&self) -> Result<Dashboard> {
        let now = Self::unix_now(&self.conn)?;
        let seconds = "CASE WHEN e.ended_at IS NULL THEN max(0, ?1 - e.started_at) ELSE e.seconds END";
        let mut stmt = self.conn.prepare("SELECT status, count(*) FROM tasks GROUP BY status")?;
        let statuses: Vec<StatusCount> = stmt
            .query_map([], |r| Ok(StatusCount { status: r.get(0)?, count: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?;
        let tasks = statuses.iter().map(|s| s.count).sum();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT p.slug, p.name, p.color, coalesce(sum({seconds}), 0)
             FROM projects p LEFT JOIN tasks t ON t.project_id = p.id
             LEFT JOIN time_entries e ON e.task_id = t.id
             GROUP BY p.id ORDER BY p.slug"
        ))?;
        let projects: Vec<ProjectTime> = stmt
            .query_map([now], |r| {
                Ok(ProjectTime { slug: r.get(0)?, name: r.get(1)?, color: r.get(2)?, seconds: r.get(3)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let total_seconds = projects.iter().map(|p| p.seconds).sum();
        let running_seconds: i64 = self.conn.query_row(
            "SELECT coalesce(sum(max(0, ?1 - started_at)), 0) FROM time_entries WHERE ended_at IS NULL",
            [now],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(&format!(
            "WITH RECURSIVE days(day) AS (
                SELECT date('now', '-6 days') UNION ALL
                SELECT date(day, '+1 day') FROM days WHERE day < date('now')
             ) SELECT days.day, coalesce(sum({seconds}), 0) FROM days
             LEFT JOIN time_entries e ON date(e.started_at, 'unixepoch') = days.day
             GROUP BY days.day ORDER BY days.day"
        ))?;
        let days: Vec<DayTime> = stmt
            .query_map([now], |r| Ok(DayTime { day: r.get(0)?, seconds: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Dashboard { statuses, projects, days, total_seconds, running_seconds, tasks })
    }

    // Comments

    pub fn list_comments(&self, task_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, task_id, author, body, created_at FROM comments WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([task_id], Self::comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn comment_row(r: &Row) -> rusqlite::Result<Comment> {
        Ok(Comment { id: r.get(0)?, task_id: r.get(1)?, author: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
    }

    /// Adds a comment, with the images (files already in `images/`) attached to it.
    /// A comment needs text, an image, or both.
    pub fn add_comment(&self, task_id: i64, author: &str, body: &str, images: &[String]) -> Result<Comment> {
        let body = body.trim();
        if body.is_empty() && images.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        if let Some(bad) = images.iter().find(|f| !valid_image_file(f)) {
            return Err(Error::Invalid(format!("{bad:?} is not an image Brokan saved")));
        }
        let author = match author.trim() {
            "" => "me",
            a => a,
        };
        self.get_task(task_id)?;
        let id = self.write(|c| {
            c.execute(
                "INSERT INTO comments (task_id, author, body) VALUES (?1, ?2, ?3)",
                params![task_id, author, body],
            )?;
            let id = c.last_insert_rowid();
            let mut insert = c.prepare("INSERT INTO images (task_id, comment_id, file) VALUES (?1, ?2, ?3)")?;
            for file in images {
                insert.execute(params![task_id, id, file])?;
            }
            Ok(id)
        })?;
        self.get_comment(id)
    }

    fn get_comment(&self, id: i64) -> Result<Comment> {
        self.conn
            .query_row(
                "SELECT id, task_id, author, body, created_at FROM comments WHERE id = ?1",
                [id],
                Self::comment_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no comment {id}")))
    }

    /// Deletes a comment; returns it as it was.
    pub fn delete_comment(&self, id: i64) -> Result<Comment> {
        let comment = self.get_comment(id)?;
        self.conn.execute("DELETE FROM comments WHERE id = ?1", [id])?;
        Ok(comment)
    }

    // Images

    /// Attaches an image, already saved in `images/`, to a task.
    pub fn add_image(&self, task_id: i64, file: &str) -> Result<Image> {
        if !valid_image_file(file) {
            return Err(Error::Invalid(format!("{file:?} is not an image Brokan saved")));
        }
        self.get_task(task_id)?;
        self.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, ?2)", params![task_id, file])?;
        Ok(Image { id: self.conn.last_insert_rowid(), task_id, comment_id: None, file: file.into() })
    }

    /// The images of a task and of its comments, oldest first.
    pub fn list_images(&self, task_id: i64) -> Result<Vec<Image>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, task_id, comment_id, file FROM images WHERE task_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([task_id], Self::image_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn image_row(r: &Row) -> rusqlite::Result<Image> {
        Ok(Image { id: r.get(0)?, task_id: r.get(1)?, comment_id: r.get(2)?, file: r.get(3)? })
    }

    /// Removes an image from its task or comment; returns it as it was.
    pub fn delete_image(&self, id: i64) -> Result<Image> {
        let image = self
            .conn
            .query_row("SELECT id, task_id, comment_id, file FROM images WHERE id = ?1", [id], Self::image_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no image {id}")))?;
        self.conn.execute("DELETE FROM images WHERE id = ?1", [id])?;
        Ok(image)
    }

    /// The image files attached to one image, comment, task (with its comments) or project.
    pub fn image_files(&self, of: ImagesOf) -> Result<Vec<String>> {
        let (sql, id) = match of {
            ImagesOf::Image(id) => ("SELECT file FROM images WHERE id = ?1", id),
            ImagesOf::Comment(id) => ("SELECT DISTINCT file FROM images WHERE comment_id = ?1", id),
            ImagesOf::Task(id) => ("SELECT DISTINCT file FROM images WHERE task_id = ?1", id),
            ImagesOf::Project(id) => {
                ("SELECT DISTINCT i.file FROM images i JOIN tasks t ON t.id = i.task_id WHERE t.project_id = ?1", id)
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Of `files`, those no task or comment uses any more.
    pub fn unused_images(&self, files: Vec<String>) -> Result<Vec<String>> {
        let mut used = self.conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM images WHERE file = ?1)")?;
        let mut unused = Vec::new();
        for file in files {
            if !used.query_row([&file], |r| r.get::<_, bool>(0))? {
                unused.push(file);
            }
        }
        Ok(unused)
    }
}

/// Whose images `Store::image_files` lists.
pub enum ImagesOf {
    Image(i64),
    Comment(i64),
    Task(i64),
    Project(i64),
}

/// The name Brokan gives a saved image: 16 hex digits and its type, like `6fec521a01297ab5.png`.
pub fn valid_image_file(file: &str) -> bool {
    match file.split_once('.') {
        Some((hash, ext)) => {
            hash.len() == 16
                && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && ["png", "jpg", "gif", "webp"].contains(&ext)
        }
        None => false,
    }
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `2026-10-25T09:30:00Z` -> `2026-10-25 09:30 UTC`.
pub fn short_time(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => ts.to_string(),
    }
}

/// `" Bug, UI,bug ,, needs  review"` -> `["bug", "ui", "needs review"]`.
fn clean_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in input.split(',') {
        let tag = squash(tag).to_lowercase();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

fn clean_repo(repo: &str) -> Result<String> {
    repo_from_url(repo).ok_or_else(|| Error::Invalid(format!("repo {:?} should look like owner/repo", repo.trim())))
}

/// `"  a   b "` -> `"a b"`: words joined by single spaces.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn clean_title(title: &str) -> Result<String> {
    let title = squash(title);
    if title.is_empty() {
        return Err(Error::Invalid("task title is empty".into()));
    }
    Ok(title)
}

/// Splits `api#12`, `#12` or `12` into an optional project slug and the task id.
fn parse_task_key(key: &str) -> Result<(Option<String>, i64)> {
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
/// A browser URL like `https://github.com/owner/repo/tree/main` gives `owner/repo` too.
pub fn repo_from_url(input: &str) -> Option<String> {
    let s = input.trim();
    let (path, bare) = if let Some((_, rest)) = s.split_once("://") {
        // scheme://[user@]host[:port]/owner/repo...
        (rest.split_once('/')?.1, false)
    } else if let Some((_, rest)) = s.split_once(':') {
        // user@host:owner/repo
        (rest, false)
    } else {
        (s, true)
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || (bare && parts.len() != 2) {
        return None;
    }
    let owner = parts[0];
    let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| format!("{owner}/{repo}").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("brokan.db"), None).unwrap();
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
    fn keeps_only_the_newest_done_tasks() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        s.create_task(p.id, "open", "").unwrap();
        for title in ["d1", "d2", "d3", "d4"] {
            let t = s.create_task(p.id, title, "").unwrap();
            s.move_task(t.id, "done", None).unwrap();
        }
        let (tasks, hidden) = s.list_tasks_keeping_done(None, None, Some(2)).unwrap();
        assert_eq!((titles(&tasks), hidden), (vec!["open", "d3", "d4"], 2));
        let (tasks, hidden) = s.list_tasks_keeping_done(Some(p.id), Some("done"), Some(0)).unwrap();
        assert_eq!((tasks.len(), hidden), (0, 4));
        assert_eq!(s.list_tasks_keeping_done(None, Some("todo"), Some(0)).unwrap().1, 0);
        let (all, hidden) = s.list_tasks_keeping_done(None, None, None).unwrap();
        assert_eq!((titles(&all), hidden), (vec!["open", "d1", "d2", "d3", "d4"], 0));
    }

    #[test]
    fn opens_a_board_from_a_newer_brokan_when_it_can() {
        let (_dir, s) = store();
        let set = |version: usize, compatible: usize| {
            s.conn
                .execute_batch(&format!(
                    "UPDATE meta SET value = '{version}' WHERE key = 'schema_version';
                     UPDATE meta SET value = '{compatible}' WHERE key = 'compatible';"
                ))
                .unwrap();
        };
        let newer = MIGRATIONS.len() + 1;
        // A newer Brokan only added things: this one goes on using the board, untouched.
        set(newer, MIGRATIONS.len());
        s.migrate().unwrap();
        s.create_project("api", "", None).unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, newer.to_string());
        // A change this version can't live with: it says how to update.
        set(newer, newer);
        assert!(s.migrate().unwrap_err().to_string().contains("brokan update"));
    }

    #[test]
    fn projects() {
        let (_dir, s) = store();
        let p = s.create_project("API", "API server", Some("https://github.com/Me/Api.git")).unwrap();
        assert_eq!((p.slug.as_str(), p.repos.as_slice()), ("api", ["me/api".to_string()].as_slice()));
        assert_eq!(s.find_project("me/api").unwrap().id, p.id);
        assert_eq!(s.project_by_repo("git@github.com:me/api.git").unwrap().unwrap().id, p.id);
        assert!(s.create_project("api", "", None).unwrap_err().to_string().contains("already exists"));
        assert!(s.create_project("web", "", Some("me/api")).unwrap_err().to_string().contains("already linked"));
        assert!(s.create_project("bad slug", "", None).is_err());
        let web = s.create_project("web", "", None).unwrap();
        assert_eq!((web.name.as_str(), web.color.as_deref(), web.icon.as_deref()), ("web", None, None));
        assert_eq!(s.update_project("web", None, Some("teal"), None).unwrap().color.as_deref(), Some("teal"));
        assert!(s.update_project("web", None, Some("#ff0000"), None).is_err());
        let renamed = s.update_project("web", Some("  Web   site "), None, None).unwrap();
        assert_eq!((renamed.slug.as_str(), renamed.name.as_str()), ("web", "Web site"));
        assert!(s.update_project("web", Some("  "), None, None).is_err());
        assert!(s.find_project("nope").unwrap_err().to_string().contains("api, web"));
    }

    #[test]
    fn project_icon_migrates_and_updates_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            let s = Store::open(&path, None).unwrap();
            s.conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..7] {
                s.conn.execute_batch(sql).unwrap();
            }
            s.conn
                .execute_batch("INSERT INTO meta (key, value) VALUES ('schema_version', '7'), ('compatible', '4')")
                .unwrap();
            s.conn.execute_batch("INSERT INTO projects (slug, name, color) VALUES ('web', 'Website', 'teal')").unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        let old = s.find_project("web").unwrap();
        assert_eq!((old.color.as_deref(), old.icon.as_deref()), (Some("teal"), None));
        let changed = s.update_project("web", Some("  New  name "), Some("red"), Some("rocket")).unwrap();
        assert_eq!(
            (changed.name.as_str(), changed.color.as_deref(), changed.icon.as_deref()),
            ("New name", Some("red"), Some("rocket"))
        );
        let before = s.changes().unwrap();
        assert!(s.update_project("web", Some("Wrong"), Some("blue"), Some("<svg>")).is_err());
        assert!(s.update_project("web", Some("Wrong"), Some("#bad"), Some("book")).is_err());
        assert_eq!(s.changes().unwrap(), before);
        let persisted = Store::open(&path, None).unwrap().find_project("web").unwrap();
        assert_eq!(
            (persisted.name, persisted.color.as_deref(), persisted.icon.as_deref()),
            ("New name".into(), Some("red"), Some("rocket"))
        );
        assert_eq!(s.update_project("web", None, None, Some("folder")).unwrap().icon.as_deref(), Some("folder"));
    }

    #[test]
    fn time_migration_preserves_existing_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            let s = Store::open(&path, None).unwrap();
            s.conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..8] {
                s.conn.execute_batch(sql).unwrap();
            }
            s.conn
                .execute_batch("INSERT INTO meta (key, value) VALUES ('schema_version', '8'), ('compatible', '4')")
                .unwrap();
            s.conn
                .execute_batch(
                    "INSERT INTO projects (slug, name) VALUES ('api', 'API');
                INSERT INTO tasks (project_id, title, status, position) VALUES (1, 'Existing', 'done', 1);",
                )
                .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        let old = s.get_task(1).unwrap();
        assert_eq!((old.title.as_str(), old.status.as_str()), ("Existing", "done"));
        assert!(s.time_report(old.id).unwrap().entries.is_empty());
        assert_eq!(s.dashboard().unwrap().tasks, 1);
        s.log_time(old.id, 30).unwrap();
        assert_eq!(Store::open(&path, None).unwrap().time_report(old.id).unwrap().total_seconds, 1800);
    }

    #[test]
    fn deadline_migrates_validates_and_clears_without_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            let s = Store::open(&path, None).unwrap();
            s.conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..10] {
                s.conn.execute_batch(sql).unwrap();
            }
            s.conn
                .execute_batch(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', '10'), ('compatible', '10');
                 INSERT INTO projects (slug, name) VALUES ('old', 'Old');
                 INSERT INTO tasks (project_id, title, status, position) VALUES (1, 'Existing', 'todo', 1);",
                )
                .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.get_task(1).unwrap().deadline_at, None);
        for bad in [
            "",
            "2025-02-29T12:00:00Z",
            "2024-04-31T12:00:00Z",
            "2024-00-01T00:00:00Z",
            "2024-02-29T24:00:00Z",
            "2024-02-29T12:60:00Z",
            "2024-02-29T12:00:00+00:00",
            "2024-02-29",
        ] {
            assert!(
                s.update_task(
                    1,
                    TaskPatch {
                        title: Some("Changed".into()),
                        deadline_at: Some(Some(bad.into())),
                        ..Default::default()
                    }
                )
                .is_err(),
                "{bad:?}"
            );
            assert!(s.create_task_with_deadline(1, None, "New", "", None, Some(bad)).is_err(), "{bad:?}");
            assert_eq!(s.get_task(1).unwrap().title, "Existing");
            assert_eq!(s.list_tasks(Some(1), None).unwrap().len(), 1);
        }
        let date = "2024-02-29T12:30:00Z";
        let t = s.create_task_with_deadline(1, None, "Scheduled", "", None, Some(date)).unwrap();
        assert_eq!(t.deadline_at.as_deref(), Some(date));
        let changed = s
            .update_task(
                1,
                TaskPatch {
                    deadline_at: Some(Some("2024-12-31T23:59:59Z".into())),
                    priority: Some("high".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(changed.deadline_at.as_deref(), Some("2024-12-31T23:59:59Z"));
        assert_eq!(
            s.update_task(1, TaskPatch { status: Some("doing".into()), ..Default::default() }).unwrap().deadline_at,
            changed.deadline_at
        );
        assert_eq!(Store::open(&path, None).unwrap().get_task(1).unwrap().deadline_at, changed.deadline_at);
        assert_eq!(
            s.update_task(1, TaskPatch { deadline_at: Some(None), ..Default::default() }).unwrap().deadline_at,
            None
        );
        assert_eq!(Store::open(&path, None).unwrap().get_task(1).unwrap().deadline_at, None);
    }

    #[test]
    fn archive_migrates_and_retains_project_tasks_time_and_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            let s = Store::open(&path, None).unwrap();
            s.conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..9] {
                s.conn.execute_batch(sql).unwrap();
            }
            s.conn
                .execute_batch("INSERT INTO meta (key, value) VALUES ('schema_version', '9'), ('compatible', '4')")
                .unwrap();
            s.conn
                .execute_batch(
                    "INSERT INTO projects (slug, name, icon) VALUES ('old', 'Old', 'rocket');
                     INSERT INTO project_repos (repo, project_id) VALUES ('me/old', 1);
                     INSERT INTO tasks (project_id, title, status, position, priority) VALUES (1, 'Saved', 'todo', 1, 'urgent');
                     INSERT INTO time_entries (task_id, started_at, ended_at, seconds) VALUES (1, 1000, 1600, 600);",
                )
                .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        assert_eq!(
            s.conn.query_row("SELECT value FROM meta WHERE key = 'compatible'", [], |r| r.get::<_, String>(0)).unwrap(),
            "10",
            "older agents must not select archived tasks after syncing"
        );
        assert!(!s.find_project("old").unwrap().archived);
        let active = s.create_project("active", "Active", None).unwrap();
        let active_task = s.create_task(active.id, "Active task", "").unwrap();
        let archived_done = s.create_task(1, "Archived done", "").unwrap();
        s.move_task(archived_done.id, "done", None).unwrap();
        let before = s.changes().unwrap();
        let archived = s.set_project_archived("old", true).unwrap();
        assert!(archived.archived);
        assert_eq!(archived.repos, ["me/old"]);
        assert_eq!(archived.icon.as_deref(), Some("rocket"));
        assert!(s.changes().unwrap() > before);
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["Active task"]);
        assert_eq!(titles(&s.list_tasks(Some(archived.id), None).unwrap()), ["Saved", "Archived done"]);
        assert_eq!(titles(&s.list_tasks_keeping_done(None, None, Some(1)).unwrap().0), ["Active task"]);
        assert_eq!(s.list_tasks_keeping_done(None, None, Some(1)).unwrap().1, 0);
        assert_eq!(s.list_tasks_keeping_done(Some(archived.id), None, Some(1)).unwrap().0.len(), 2);
        assert!(s.search("Saved", 10).unwrap().is_empty());
        assert!(s.create_task(archived.id, "Rejected", "").is_err());
        let patch = TaskPatch { title: Some("Wrong".into()), project_id: Some(archived.id), ..Default::default() };
        assert!(s.update_task(active_task.id, patch).is_err());
        assert_eq!(s.get_task(active_task.id).unwrap().title, "Active task");
        assert!(s.next_task(Some(archived.id)).unwrap().is_none());
        assert_eq!(s.next_task(None).unwrap().unwrap().id, active_task.id);
        assert_eq!(s.time_report(1).unwrap().total_seconds, 600);
        assert_eq!(s.dashboard().unwrap().total_seconds, 600);
        assert_eq!(s.set_project_archived("old", true).unwrap().total, 2);
        assert!(Store::open(&path, None).unwrap().find_project("old").unwrap().archived);
        s.set_project_archived("old", false).unwrap();
        assert!(s.search("Saved", 10).unwrap().len() == 1);
        assert_eq!(s.next_task(None).unwrap().unwrap().title, "Saved");
        assert_eq!(s.create_task(archived.id, "Restored", "").unwrap().project, "old");
    }

    #[test]
    fn timer_and_dashboard_account_for_live_and_manual_time() {
        let (dir, s) = store();
        let api = s.create_project("api", "API", None).unwrap();
        let web = s.create_project("web", "Website", None).unwrap();
        let first = s.create_task(api.id, "First", "").unwrap();
        let second = s.create_task(web.id, "Second", "").unwrap();
        s.move_task(second.id, "done", None).unwrap();
        assert_eq!(s.dashboard().unwrap().tasks, 2);
        assert!(s.log_time(first.id, 0).is_err());
        assert!(s.log_time(first.id, 1441).is_err());
        assert!(s.time_report(first.id).unwrap().entries.is_empty());
        assert_eq!(s.log_time(first.id, 30).unwrap().total_seconds, 1800);
        assert_eq!(s.log_time(second.id, 20).unwrap().total_seconds, 1200);
        let before = s.changes().unwrap();
        let running = s.start_timer(first.id).unwrap();
        assert!(running.running_since.is_some());
        assert_eq!(running.entries.len(), 2);
        assert_eq!(
            Store::open(&dir.path().join("brokan.db"), None).unwrap().time_report(first.id).unwrap().running_since,
            running.running_since,
            "timer survives reopening the board"
        );
        assert!(s.delete_time(first.id, running.entries[0].id).is_err(), "running entries cannot be deleted");
        s.start_timer(first.id).unwrap();
        assert_eq!(s.time_report(first.id).unwrap().entries.len(), 2);
        assert!(s.start_timer(second.id).unwrap_err().to_string().contains("stop it first"));
        assert_eq!(s.changes().unwrap(), before + 1, "only the first start writes");
        let current = s.dashboard().unwrap();
        assert_eq!((current.tasks, current.projects.len(), current.days.len()), (2, 2, 7));
        assert_eq!(current.total_seconds, 3000 + current.running_seconds);
        assert_eq!(current.days.iter().map(|d| d.seconds).sum::<i64>(), current.total_seconds);
        assert_eq!(current.projects.iter().find(|p| p.slug == "api").unwrap().seconds, 1800 + current.running_seconds);
        let stopped = s.stop_timer(first.id).unwrap();
        assert!(stopped.running_since.is_none());
        assert!(stopped.total_seconds >= 1801);
        assert_eq!(s.stop_timer(first.id).unwrap().total_seconds, stopped.total_seconds);
        let old = s.time_report(first.id).unwrap().entries.last().unwrap().id;
        assert!(s.delete_time(second.id, old).is_err(), "wrong task must not delete an entry");
        assert_eq!(s.delete_time(first.id, old).unwrap().total_seconds, stopped.total_seconds - 1800);
        assert!(s.delete_time(first.id, old).is_err());
        let completed = s.time_report(first.id).unwrap().entries[0].id;
        s.conn
            .execute(
                "UPDATE time_entries SET started_at = unixepoch('now', '-9 days'),
                ended_at = unixepoch('now', '-9 days') WHERE id = ?1",
                [completed],
            )
            .unwrap();
        let historical = s.dashboard().unwrap();
        assert!(
            historical.days.iter().map(|d| d.seconds).sum::<i64>() < historical.total_seconds,
            "seven-day bars exclude older work but all-time totals include it"
        );
        s.delete_task(first.id).unwrap();
        assert_eq!(s.dashboard().unwrap().total_seconds, 1200);
        assert!(s.time_report(first.id).is_err());
    }

    #[test]
    fn many_repos_per_project() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        s.create_project("web", "", None).unwrap();
        assert_eq!(s.link_repo("api", "https://github.com/Me/API-docs.git").unwrap().repos, ["me/api", "me/api-docs"]);
        assert_eq!(s.find_project("me/api-docs").unwrap().id, api.id);
        assert_eq!(s.link_repo("api", "me/api").unwrap().repos.len(), 2, "linking twice changes nothing");
        assert!(s.link_repo("web", "me/api-docs").unwrap_err().to_string().contains("already linked to project api"));
        assert!(s.link_repo("api", "not a repo").is_err());
        assert_eq!(s.unlink_repo("api", "me/api").unwrap().repos, ["me/api-docs"]);
        assert!(s.project_by_repo("me/api").unwrap().is_none());
        assert!(s.unlink_repo("api", "me/api").is_err());
        // A deleted project frees its repos.
        s.delete_project("api").unwrap();
        assert_eq!(s.link_repo("web", "me/api-docs").unwrap().repos, ["me/api-docs"]);
    }

    #[test]
    fn search() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let login = s.create_task(api.id, "Fix login", "Users loop on /login").unwrap();
        let old = s.create_task(web.id, "Login page copy", "").unwrap();
        s.update_task(old.id, TaskPatch { status: Some("done".into()), ..Default::default() }).unwrap();
        let other = s.create_task(web.id, "Pricing 100%", "").unwrap();
        s.update_task(other.id, TaskPatch { tags: Some("ui".into()), ..Default::default() }).unwrap();
        let titles = |q: &str| s.search(q, 10).unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles("LOGIN"), ["Fix login", "Login page copy"], "any project, open first, done too");
        assert_eq!(titles("login api"), ["Fix login"], "every word must match, the project too");
        assert_eq!(titles(&format!("web#{}", other.id)), ["Pricing 100%"], "by key");
        assert_eq!(titles("ui"), ["Pricing 100%"], "by tag");
        assert_eq!(titles("100%"), ["Pricing 100%"], "% is a plain character");
        assert!(titles("nothing-like-this").is_empty());
        assert_eq!(s.search("", 10).unwrap().len(), 3, "no words: the latest changes");
        assert_eq!(s.search("login", 1).unwrap()[0].id, login.id);
    }

    #[test]
    fn images() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "Bug", "").unwrap();
        let shot = "0123456789abcdef.png".to_string();
        let on_task = s.add_image(t.id, &shot).unwrap();
        assert!(s.add_image(t.id, "../../etc/passwd").is_err());
        assert!(s.add_image(t.id + 1, &shot).is_err());
        // A comment can be only images.
        let c = s.add_comment(t.id, "me", "", &["fedcba9876543210.jpg".into(), shot.clone()]).unwrap();
        assert!(s.add_comment(t.id, "me", "x", &["evil.svg".into()]).is_err());
        let all: Vec<_> = s.list_images(t.id).unwrap().into_iter().map(|i| (i.comment_id, i.file)).collect();
        assert_eq!(
            all,
            [(None, shot.clone()), (Some(c.id), "fedcba9876543210.jpg".into()), (Some(c.id), shot.clone())]
        );
        assert_eq!(s.image_files(ImagesOf::Task(t.id)).unwrap(), [shot.clone(), "fedcba9876543210.jpg".into()]);
        assert_eq!(s.image_files(ImagesOf::Project(p.id)).unwrap().len(), 2);
        // The same file twice: removing one attachment leaves it in use.
        s.delete_image(on_task.id).unwrap();
        assert!(s.unused_images(vec![shot.clone()]).unwrap().is_empty());
        let files = s.image_files(ImagesOf::Comment(c.id)).unwrap();
        s.delete_comment(c.id).unwrap();
        assert!(s.list_images(t.id).unwrap().is_empty());
        assert_eq!(s.unused_images(files).unwrap(), ["fedcba9876543210.jpg".to_string(), shot.clone()]);
        assert!(valid_image_file("6fec521a01297ab5.webp"));
        assert!(!valid_image_file("6FEC521A01297AB5.png") && !valid_image_file("6fec521a01297ab5.svg"));
    }

    #[test]
    fn migration_moves_repos_to_their_own_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            // A board at schema version 3, from before projects could have several repos.
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..3] {
                c.execute_batch(sql).unwrap();
            }
            c.execute_batch(
                "INSERT INTO meta VALUES ('schema_version', '3');
                 INSERT INTO projects (slug, name, repo) VALUES ('api', 'api', 'me/api'), ('web', 'web', NULL);",
            )
            .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        assert_eq!(s.find_project("api").unwrap().repos, ["me/api"]);
        assert!(s.find_project("web").unwrap().repos.is_empty());
        assert_eq!(s.find_project("me/api").unwrap().slug, "api");
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
        let web = s.create_project("web", "", None).unwrap();
        let moved = s.update_task(t.id, TaskPatch { project_id: Some(web.id), ..Default::default() }).unwrap();
        assert_eq!((moved.key(), moved.status.as_str()), (format!("web#{}", t.id), "review"));
        let t = s.update_task(t.id, TaskPatch { project_id: Some(p.id), ..Default::default() }).unwrap();

        s.add_comment(t.id, "claude-code", "Done, see PR", &[]).unwrap();
        s.add_comment(t.id, "", "thanks", &[]).unwrap();
        let comments = s.list_comments(t.id).unwrap();
        assert_eq!(comments.iter().map(|c| c.author.as_str()).collect::<Vec<_>>(), ["claude-code", "me"]);
        assert!(s.add_comment(t.id, "me", " ", &[]).is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        let tagged =
            s.update_task(t.id, TaskPatch { tags: Some(" Bug, UI,bug ,, needs  review".into()), ..Default::default() });
        assert_eq!(tagged.unwrap().tags, ["bug", "ui", "needs review"]);
        assert!(
            s.update_task(t.id, TaskPatch { tags: Some(" , ".into()), ..Default::default() }).unwrap().tags.is_empty()
        );
        let gone = s.add_comment(t.id, "me", "typo", &[]).unwrap();
        assert_eq!(s.delete_comment(gone.id).unwrap().body, "typo");
        assert!(s.delete_comment(gone.id).is_err());
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
    fn delete_project_removes_its_tasks_and_comments() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let t = s.create_task(api.id, "a", "").unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
        let kept = s.create_task(web.id, "b", "").unwrap();
        s.add_comment(kept.id, "me", "hello", &[]).unwrap();

        assert_eq!(s.delete_project("api").unwrap().id, api.id);
        assert!(s.find_project("api").is_err());
        assert!(s.get_task(t.id).is_err());
        let count = |table: &str| -> i64 {
            s.conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((count("projects"), count("tasks"), count("comments")), (1, 1, 1));
        assert!(s.delete_project("api").is_err());
        // The slug and repo are free again.
        s.create_project("api", "", Some("me/api")).unwrap();
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
    fn priority_migrates_and_validates_without_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokan.db");
        {
            let s = Store::open(&path, None).unwrap();
            // Model a board saved at version 6, before the additive priority migration.
            s.conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..6] {
                s.conn.execute_batch(sql).unwrap();
            }
            s.conn
                .execute_batch(
                    "INSERT INTO meta (key, value) VALUES
                ('schema_version', '6'), ('compatible', '4')",
                )
                .unwrap();
            s.conn
                .execute_batch(
                    "INSERT INTO projects (slug, name) VALUES ('api', 'api');
                INSERT INTO tasks (project_id, title, status, position)
                VALUES ((SELECT id FROM projects WHERE slug = 'api'), 'old', 'todo', 1);",
                )
                .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        let old = s.list_tasks(None, None).unwrap().pop().unwrap();
        assert_eq!(old.priority, "none");
        let before = s.changes().unwrap();
        assert!(s.create_task_in(s.find_project("api").unwrap().id, None, "bad", "", Some("extreme")).is_err());
        assert!(
            s.update_task(
                old.id,
                TaskPatch { title: Some("changed".into()), priority: Some("extreme".into()), ..Default::default() }
            )
            .is_err()
        );
        assert_eq!(s.changes().unwrap(), before);
        assert_eq!(s.get_task(old.id).unwrap().title, "old");
        assert_eq!(
            s.update_task(old.id, TaskPatch { priority: Some("high".into()), ..Default::default() }).unwrap().priority,
            "high"
        );
        assert_eq!(s.search("old", 1).unwrap()[0].priority, "high");
        assert_eq!(
            s.update_task(old.id, TaskPatch { priority: Some("none".into()), ..Default::default() }).unwrap().priority,
            "none"
        );
    }

    #[test]
    fn next_task_prefers_priority_then_board_position() {
        let (dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let none = s.create_task(api.id, "none", "").unwrap();
        let low = s.create_task_in(web.id, None, "low", "", Some("low")).unwrap();
        let high = s.create_task_in(api.id, None, "high", "", Some("high")).unwrap();
        let urgent = s.create_task_in(web.id, None, "urgent", "", Some("urgent")).unwrap();
        let top = s.create_task_in(api.id, None, "top", "", Some("high")).unwrap();
        s.move_task(top.id, "todo", Some(none.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["top", "none", "low", "high", "urgent"]);
        let human = s.create_task_in(api.id, None, "human", "", Some("urgent")).unwrap();
        s.update_task(human.id, TaskPatch { human: Some(true), ..Default::default() }).unwrap();
        s.set_backlog(true).unwrap();
        let s = Store::open(&dir.path().join("brokan.db"), None).unwrap();
        s.create_task_in(api.id, Some("backlog"), "backlog", "", Some("urgent")).unwrap();
        assert_eq!(s.next_task(Some(api.id)).unwrap().unwrap().id, top.id);
        assert_eq!(s.next_task(None).unwrap().unwrap().id, urgent.id);
        assert_eq!(s.next_task(None).unwrap().unwrap().id, high.id);
        assert_eq!(s.next_task(None).unwrap().unwrap().id, low.id);
        assert_eq!(s.next_task(None).unwrap().unwrap().id, none.id);
        assert!(s.next_task(None).unwrap().is_none());
    }

    #[test]
    fn next_task_takes_top_todo() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "web first", "").unwrap();
        // At the top of api's todo, but for a person: agents never get it.
        let human = s.create_task(api.id, "Call the bank", "").unwrap();
        assert!(s.update_task(human.id, TaskPatch { human: Some(true), ..Default::default() }).unwrap().human);
        s.create_task(api.id, "api first", "").unwrap();
        s.create_task(api.id, "api second", "").unwrap();

        let t = s.next_task(Some(api.id)).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("api first", "doing"));
        assert_eq!(s.next_task(None).unwrap().unwrap().title, "web first");
        assert_eq!(s.next_task(Some(api.id)).unwrap().unwrap().title, "api second");
        assert!(s.next_task(Some(api.id)).unwrap().is_none());
        assert_eq!(s.get_task(human.id).unwrap().status, "todo");
    }

    #[test]
    fn backlog_is_a_column_agents_skip() {
        let (dir, s) = store();
        assert_eq!(s.statuses(), STATUSES);
        s.set_backlog(true).unwrap();
        let s = Store::open(&dir.path().join("brokan.db"), None).unwrap();
        assert_eq!(s.statuses(), ["backlog", "todo", "doing", "review", "done"]);
        let api = s.create_project("api", "", None).unwrap();
        let backlog = s.create_task_in(api.id, Some("backlog"), "later", "", None).unwrap();
        assert_eq!(backlog.status, "backlog");
        assert!(s.create_task_in(api.id, Some("nope"), "x", "", None).is_err());
        assert_eq!(s.create_task(api.id, "now", "").unwrap().status, "todo");

        let t = s.next_task(None).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("now", "doing"));
        assert!(s.next_task(None).unwrap().is_none());
        assert_eq!(s.get_task(backlog.id).unwrap().status, "backlog");

        // Hiding it would hide its tasks.
        assert!(s.set_backlog(false).is_err());
        s.delete_task(backlog.id).unwrap();
        s.set_backlog(false).unwrap();
    }

    #[test]
    fn change_counter_moves_on_every_write() {
        let (_dir, s) = store();
        let c0 = s.changes().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let c1 = s.changes().unwrap();
        let t = s.create_task(p.id, "x", "").unwrap();
        let c2 = s.changes().unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
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
            "https://github.com/owner/repo/",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo/pull/3",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("owner/repo"), "{url}");
        }
        assert_eq!(repo_from_url("repo"), None);
        assert_eq!(repo_from_url("a/b/c"), None);
        assert_eq!(repo_from_url("own er/repo"), None);
    }
}
