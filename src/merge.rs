//! Three-way merge of two boards that both changed since they last synced.
//!
//! `brokan.db` is one SQLite file, which git cannot merge, so sync merges the boards
//! themselves: it starts from the board in the repo (`remote`) and replays on it what
//! this machine changed since the last common sync (`base` → `local`).
//!
//! - Rows are matched by id. Ids up to the base's last id existed when both sides
//!   last agreed; higher ids are new on that side. The repo's new rows keep their ids,
//!   which other machines may already know; this machine's new rows keep theirs too
//!   unless the repo used the same id, and then get the next free one. References to
//!   them (comments of a renumbered task, say) follow.
//! - Edits merge field by field: a field only one side changed takes that change. When
//!   both changed the same field differently, the task edited last wins (by
//!   `updated_at`); in other tables this machine's change wins.
//! - A row deleted on one side is deleted only if the other side left it, and
//!   everything under it, as it was. Otherwise the edited side keeps it.
//! - Projects new on both sides with the same slug become one project.
//!
//! Nothing ever stops on a conflict: every case has an answer.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

/// A table keyed by `id`, and the columns that point at its parents.
struct Table {
    name: &'static str,
    parents: &'static [(&'static str, &'static str)],
}

/// Parents before children, so references resolve as rows arrive.
const TABLES: &[Table] = &[
    Table { name: "projects", parents: &[] },
    Table { name: "tasks", parents: &[("project_id", "projects")] },
    Table { name: "comments", parents: &[("task_id", "tasks")] },
    Table { name: "images", parents: &[("task_id", "tasks"), ("comment_id", "comments")] },
    Table { name: "time_entries", parents: &[("task_id", "tasks")] },
    Table { name: "automatic_time_entries", parents: &[("task_id", "tasks")] },
];

/// What the merge did that the person syncing should hear about.
#[derive(Debug, Default)]
pub struct Report {
    /// Tasks from this machine that took new ids: (old key, new key).
    pub renumbered: Vec<(String, String)>,
}

impl Report {
    pub fn summary(&self) -> String {
        if self.renumbered.is_empty() {
            return String::new();
        }
        let moves: Vec<String> = self.renumbered.iter().map(|(old, new)| format!("{old} is now {new}")).collect();
        format!(" Both machines created tasks with the same numbers, so this machine's moved: {}.", moves.join(", "))
    }
}

struct Merge<'a> {
    c: &'a Connection,
    /// Column lists shared by all three boards, `id` first.
    cols: HashMap<&'static str, Vec<String>>,
    /// The base's last id per table: higher ids are new on their side.
    base_seq: HashMap<&'static str, i64>,
    /// This machine's ids → merged ids, for rows that moved.
    map: HashMap<&'static str, HashMap<i64, i64>>,
    report: Report,
}

/// Merges into `out`, a copy of the repo's board, what changed from `base` to `local`.
/// All three must be at the same schema (`Store::migrate`). `out` is written in one
/// transaction: it is either fully merged or left as it was.
pub fn merge(out: &Path, base: &Path, local: &Path) -> Result<Report> {
    let c = Connection::open(out)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.execute("ATTACH DATABASE ?1 AS base", [base.to_string_lossy()])?;
    c.execute("ATTACH DATABASE ?1 AS local", [local.to_string_lossy()])?;
    c.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        // Triggers would move clocks and counters while rows are copied; they come back
        // unchanged at the end of the same transaction.
        let triggers: Vec<(String, String)> = c
            .prepare("SELECT name, sql FROM main.sqlite_master WHERE type = 'trigger' AND sql IS NOT NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, _) in &triggers {
            c.execute_batch(&format!("DROP TRIGGER main.{}", quote(name)))?;
        }
        let mut m = Merge::new(&c)?;
        for table in TABLES {
            m.table(table)?;
            if table.name == "projects" {
                m.repos()?;
            }
        }
        m.meta()?;
        m.settle_clocks()?;
        for (_, sql) in &triggers {
            c.execute_batch(sql)?;
        }
        c.execute("UPDATE main.meta SET value = value + 1 WHERE key = 'changes'", [])?;
        Ok(m.report)
    })();
    match result {
        Ok(report) => {
            c.execute_batch("COMMIT")?;
            Ok(report)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn columns(c: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    let mut stmt = c.prepare(&format!("PRAGMA {schema}.table_info({})", quote(table)))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

fn seq(c: &Connection, schema: &str, table: &str) -> Result<i64> {
    // A table nobody ever inserted into has no row in sqlite_sequence.
    let seq: Option<i64> = c
        .query_row(&format!("SELECT seq FROM {schema}.sqlite_sequence WHERE name = ?1"), [table], |r| r.get(0))
        .optional()?;
    let max: Option<i64> = c.query_row(&format!("SELECT max(id) FROM {schema}.{}", quote(table)), [], |r| r.get(0))?;
    Ok(seq.unwrap_or(0).max(max.unwrap_or(0)))
}

impl<'a> Merge<'a> {
    fn new(c: &'a Connection) -> Result<Self> {
        let mut cols = HashMap::new();
        let mut base_seq = HashMap::new();
        for t in TABLES {
            let main = columns(c, "main", t.name)?;
            let base: HashSet<String> = columns(c, "base", t.name)?.into_iter().collect();
            let local: HashSet<String> = columns(c, "local", t.name)?.into_iter().collect();
            // A newer Brokan may have added columns: those keep the repo's values.
            let shared: Vec<String> =
                main.into_iter().filter(|col| base.contains(col) && local.contains(col)).collect();
            if shared.first().map(String::as_str) != Some("id") {
                anyhow::bail!("cannot merge boards: table {} has no id column", t.name);
            }
            cols.insert(t.name, shared);
            base_seq.insert(t.name, seq(c, "base", t.name)?);
        }
        Ok(Self { c, cols, base_seq, map: HashMap::new(), report: Report::default() })
    }

    fn select(&self, table: &str) -> String {
        self.cols[table].iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
    }

    fn rows(&self, schema: &str, table: &'static str) -> Result<Vec<Vec<Value>>> {
        let n = self.cols[table].len();
        let mut stmt =
            self.c.prepare(&format!("SELECT {} FROM {schema}.{} ORDER BY id", self.select(table), quote(table)))?;
        let rows = stmt
            .query_map([], |r| (0..n).map(|i| r.get::<_, Value>(i)).collect::<rusqlite::Result<Vec<_>>>())?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn row(&self, schema: &str, table: &'static str, id: i64) -> Result<Option<Vec<Value>>> {
        let n = self.cols[table].len();
        Ok(self
            .c
            .query_row(
                &format!("SELECT {} FROM {schema}.{} WHERE id = ?1", self.select(table), quote(table)),
                [id],
                |r| (0..n).map(|i| r.get::<_, Value>(i)).collect::<rusqlite::Result<Vec<_>>>(),
            )
            .optional()?)
    }

    fn exists(&self, table: &str, id: i64) -> Result<bool> {
        Ok(self.c.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM main.{} WHERE id = ?1)", quote(table)),
            [id],
            |r| r.get(0),
        )?)
    }

    fn col(&self, table: &str, name: &str) -> Option<usize> {
        self.cols[table].iter().position(|c| c == name)
    }

    /// True when `schema` changed anything under this row since the base: a child
    /// added, edited, or with changes of its own further down.
    fn children_changed(&self, schema: &str, table: &'static str, id: i64) -> Result<bool> {
        for child in TABLES {
            for (fk, parent) in child.parents {
                if *parent != table {
                    continue;
                }
                let ids: Vec<i64> = self
                    .c
                    .prepare(&format!("SELECT id FROM {schema}.{} WHERE {} = ?1", quote(child.name), quote(fk)))?
                    .query_map([id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for child_id in ids {
                    if self.row(schema, child.name, child_id)? != self.row("base", child.name, child_id)?
                        || self.children_changed(schema, child.name, child_id)?
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// This machine's row with its references turned into merged ids, or `None` when
    /// a row it points at did not survive the merge.
    fn mapped(
        &self,
        table: &'static str,
        parents: &[(&str, &'static str)],
        row: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let mut out = row.to_vec();
        for (fk, parent) in parents {
            let Some(i) = self.col(table, fk) else { continue };
            let Value::Integer(v) = row[i] else { continue };
            let target = match self.map.get(parent).and_then(|m| m.get(&v)) {
                Some(&new) => new,
                // A parent new on this machine that was not merged: never point at
                // whatever the repo has under the same id.
                None if v > self.base_seq[parent] => return Ok(None),
                None => v,
            };
            if !self.exists(parent, target)? {
                return Ok(None);
            }
            out[i] = Value::Integer(target);
        }
        Ok(Some(out))
    }

    /// When both changed a field differently: the newer task wins, else this machine.
    fn local_wins(&self, table: &str, local: &[Value], remote: &[Value]) -> bool {
        match self.col(table, "updated_at") {
            Some(i) => match (&local[i], &remote[i]) {
                (Value::Text(l), Value::Text(r)) => l >= r,
                _ => true,
            },
            None => true,
        }
    }

    fn table(&mut self, t: &Table) -> Result<()> {
        let name = t.name;
        let base_seq = self.base_seq[name];
        let main_seq = seq(self.c, "main", name)?;
        let base: HashMap<i64, Vec<Value>> = self.rows("base", name)?.into_iter().map(|r| (id_of(&r), r)).collect();
        let local = self.rows("local", name)?;
        let local_ids: HashSet<i64> = local.iter().map(|r| id_of(r)).collect();

        // Rows both sides had: replay this machine's edits, or bring back what the repo
        // deleted while this machine was still working on it.
        for l in local.iter().filter(|r| id_of(r) <= base_seq) {
            let id = id_of(l);
            let Some(b) = base.get(&id) else { continue };
            let edited = l != b;
            if !edited && !self.children_changed("local", name, id)? {
                continue;
            }
            let Some(lm) = self.mapped(name, t.parents, l)? else { continue };
            match self.row("main", name, id)? {
                None => {
                    self.insert(name, lm, true)?;
                }
                Some(r) if edited => {
                    let wins = self.local_wins(name, l, &r);
                    let mut merged = r.clone();
                    for i in 1..merged.len() {
                        let (lc, rc) = (l[i] != b[i], r[i] != b[i]);
                        if lc && (!rc || wins) {
                            merged[i] = lm[i].clone();
                        }
                    }
                    if merged != r && !self.update(name, &merged)? && wins {
                        // The fields don't fit together (a start after the deadline,
                        // say): take this machine's whole row instead.
                        self.update(name, &lm)?;
                    }
                }
                Some(_) => {}
            }
        }

        // Rows this machine deleted: gone, unless the repo changed them meanwhile.
        for (id, b) in &base {
            if local_ids.contains(id) {
                continue;
            }
            if self.row("main", name, *id)?.as_ref() == Some(b) && !self.children_changed("main", name, *id)? {
                self.c.execute(&format!("DELETE FROM main.{} WHERE id = ?1", quote(name)), [id])?;
            }
        }

        // Rows new on this machine.
        let mut moved = HashMap::new();
        for l in local.iter().filter(|r| id_of(r) > base_seq) {
            let id = id_of(l);
            let Some(lm) = self.mapped(name, t.parents, l)? else { continue };
            if name == "projects"
                && let Some(i) = self.col(name, "slug")
                && let Some(same) = self
                    .c
                    .query_row("SELECT id FROM main.projects WHERE slug = ?1", [&lm[i]], |r| r.get::<_, i64>(0))
                    .optional()?
            {
                moved.insert(id, same);
                self.map.entry(name).or_default().insert(id, same);
                continue;
            }
            let keep = id > main_seq && !self.exists(name, id)?;
            if let Some(new) = self.insert(name, lm, keep)? {
                if new != id {
                    moved.insert(id, new);
                }
                self.map.entry(name).or_default().insert(id, new);
            }
        }
        if name == "tasks" {
            let c = self.c;
            for (old, new) in moved {
                let slug = |schema: &str, id: i64| -> Result<String> {
                    Ok(c
                        .query_row(
                            &format!("SELECT p.slug FROM {schema}.tasks t JOIN {schema}.projects p ON p.id = t.project_id WHERE t.id = ?1"),
                            [id],
                            |r| r.get(0),
                        )
                        .optional()?
                        .unwrap_or_default())
                };
                self.report
                    .renumbered
                    .push((format!("{}#{old}", slug("local", old)?), format!("{}#{new}", slug("main", new)?)));
            }
            self.report.renumbered.sort();
        }
        Ok(())
    }

    /// Inserts `row`, under its own id when `keep_id`, and returns the id it got.
    /// A row that breaks a rule of the board (a second running timer) is adjusted
    /// first; one that still can't be stored is left out rather than failing the sync.
    fn insert(&self, table: &'static str, mut row: Vec<Value>, keep_id: bool) -> Result<Option<i64>> {
        if !keep_id {
            row[0] = Value::Null;
        }
        self.one_running(table, &mut row)?;
        let cols = self.select(table);
        let marks = vec!["?"; row.len()].join(", ");
        let sql = format!("INSERT INTO main.{} ({cols}) VALUES ({marks})", quote(table));
        match self.c.execute(&sql, params_from_iter(row.iter())) {
            Ok(_) => Ok(Some(self.c.last_insert_rowid())),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `row` over the row with its id; false when the board's rules refuse it.
    fn update(&self, table: &'static str, row: &[Value]) -> Result<bool> {
        let mut row = row.to_vec();
        self.one_running(table, &mut row)?;
        let sets: Vec<String> = self.cols[table][1..].iter().map(|c| format!("{} = ?", quote(c))).collect();
        let sql = format!("UPDATE main.{} SET {} WHERE id = ?", quote(table), sets.join(", "));
        let values = row[1..].iter().chain(std::iter::once(&row[0]));
        match self.c.execute(&sql, params_from_iter(values)) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// One manual timer runs across the board, and one activity interval per task.
    /// When the repo already has that one running, this machine's is ended.
    fn one_running(&self, table: &'static str, row: &mut [Value]) -> Result<()> {
        let automatic = match table {
            "time_entries" => false,
            "automatic_time_entries" => true,
            _ => return Ok(()),
        };
        let (Some(ended), Some(started), Some(seconds), Some(task)) = (
            self.col(table, "ended_at"),
            self.col(table, "started_at"),
            self.col(table, "seconds"),
            self.col(table, "task_id"),
        ) else {
            return Ok(());
        };
        if row[ended] != Value::Null {
            return Ok(());
        }
        let scope = if automatic { "AND task_id = ?2" } else { "AND ?2 IS NOT NULL" };
        let running: bool = self.c.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM main.{} WHERE ended_at IS NULL AND id IS NOT ?1 {scope})",
                quote(table)
            ),
            params![row[0], row[task]],
            |r| r.get(0),
        )?;
        if !running {
            return Ok(());
        }
        let Value::Integer(start) = row[started] else { return Ok(()) };
        let seen = self.col(table, "last_seen_at");
        let stop = match seen.map(|i| &row[i]) {
            Some(Value::Integer(seen)) if automatic => *seen,
            _ => self.c.query_row("SELECT unixepoch()", [], |r| r.get(0))?,
        }
        .max(start + 1);
        row[ended] = Value::Integer(stop);
        row[seconds] = Value::Integer(stop - start);
        if let Some(i) = seen.filter(|_| automatic) {
            row[i] = Value::Integer(stop);
        }
        Ok(())
    }

    /// Repo links: which project a repo belongs to, merged the same way as fields.
    fn repos(&mut self) -> Result<()> {
        let read = |schema: &str| -> Result<HashMap<String, i64>> {
            Ok(self
                .c
                .prepare(&format!("SELECT repo, project_id FROM {schema}.project_repos"))?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let (base, local, main) = (read("base")?, read("local")?, read("main")?);
        let projects = self.map.get("projects").cloned().unwrap_or_default();
        let project_seq = self.base_seq["projects"];
        for (repo, &pid) in &local {
            if base.get(repo) == Some(&pid) {
                continue;
            }
            let target = match projects.get(&pid) {
                Some(&p) => p,
                None if pid > project_seq => continue,
                None => pid,
            };
            if !self.exists("projects", target)? {
                continue;
            }
            match main.get(repo) {
                // The repo changed it too, to something else: the repo's link stays.
                Some(&current) if base.get(repo) != Some(&current) => {}
                Some(_) => {
                    self.c.execute(
                        "UPDATE main.project_repos SET project_id = ?1 WHERE repo = ?2",
                        params![target, repo],
                    )?;
                }
                None => {
                    self.c.execute(
                        "INSERT OR IGNORE INTO main.project_repos (repo, project_id) VALUES (?1, ?2)",
                        params![repo, target],
                    )?;
                }
            }
        }
        for (repo, pid) in &base {
            if !local.contains_key(repo) && main.get(repo) == Some(pid) {
                self.c.execute("DELETE FROM main.project_repos WHERE repo = ?1", [repo])?;
            }
        }
        Ok(())
    }

    /// Board settings kept in meta: only the backlog switch is shared.
    fn meta(&self) -> Result<()> {
        let get = |schema: &str| -> Result<Option<String>> {
            Ok(self
                .c
                .query_row(&format!("SELECT value FROM {schema}.meta WHERE key = 'backlog'"), [], |r| r.get(0))
                .optional()?)
        };
        let local = get("local")?;
        if local != get("base")?
            && let Some(value) = local
        {
            self.c.execute(
                "INSERT INTO main.meta (key, value) VALUES ('backlog', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [value],
            )?;
        }
        // Tasks the merge left in a hidden backlog would vanish from the board.
        let hidden: bool = self.c.query_row(
            "SELECT coalesce((SELECT value FROM main.meta WHERE key = 'backlog'), '0') <> '1'
               AND EXISTS (SELECT 1 FROM main.tasks WHERE status = 'backlog')",
            [],
            |r| r.get(0),
        )?;
        if hidden {
            self.c.execute(
                "INSERT INTO main.meta (key, value) VALUES ('backlog', '1')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )?;
        }
        Ok(())
    }

    /// The rules the triggers keep while the board is used: no manual timer on a doing
    /// task, and activity time only for doing tasks in active projects.
    fn settle_clocks(&self) -> Result<()> {
        if self.col("time_entries", "ended_at").is_some() {
            self.c.execute_batch(
                "UPDATE main.time_entries SET ended_at = max(started_at + 1, unixepoch()),
                   seconds = max(started_at + 1, unixepoch()) - started_at
                 WHERE ended_at IS NULL AND task_id IN (SELECT id FROM main.tasks WHERE status = 'doing');",
            )?;
        }
        if self.col("automatic_time_entries", "last_seen_at").is_some() {
            self.c.execute_batch(
                "UPDATE main.automatic_time_entries SET ended_at = max(started_at + 1, last_seen_at),
                   seconds = max(started_at + 1, last_seen_at) - started_at,
                   last_seen_at = max(started_at + 1, last_seen_at)
                 WHERE ended_at IS NULL AND task_id NOT IN (
                   SELECT t.id FROM main.tasks t JOIN main.projects p ON p.id = t.project_id
                   WHERE t.status = 'doing' AND p.archived = 0 AND t.human = 0);",
            )?;
        }
        Ok(())
    }
}

fn id_of(row: &[Value]) -> i64 {
    match row[0] {
        Value::Integer(id) => id,
        _ => 0,
    }
}

/// A copy of the board at `path`, for the merge to read or write without touching it.
pub fn copy_board(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to).with_context(|| format!("cannot copy {}", from.display()))?;
    Ok(())
}
