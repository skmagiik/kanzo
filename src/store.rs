//! The database uses a rollback journal (`journal_mode = TRUNCATE`), not WAL. With a
//! connection per operation, WAL would set up and tear down its `-wal` and `-shm` files
//! on every request; the rollback journal makes a read touch no extra files at all and
//! measured about 3x faster per operation.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// The oldest schema version whose Kanzo can still use a database at the latest one.
/// Adding a table or a column with a default leaves older versions working, so a board
/// synced from a newer Kanzo still opens in an older one. Raise it to the new version
/// only for a migration that older code can't live with, like a renamed column.
const COMPATIBLE_FROM: usize = 4;

const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    "ALTER TABLE projects ADD COLUMN color TEXT;",
    "ALTER TABLE tasks ADD COLUMN tags TEXT NOT NULL DEFAULT '';",
    // The old projects.repo column stays, empty: SQLite can only drop it by rebuilding the table.
    "CREATE TABLE project_repos (
         repo       TEXT PRIMARY KEY,
         project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE
     );
     CREATE INDEX project_repos_by_project ON project_repos(project_id);
     INSERT INTO project_repos (repo, project_id) SELECT repo, id FROM projects WHERE repo IS NOT NULL;
     UPDATE projects SET repo = NULL;
     CREATE TRIGGER project_repos_insert AFTER INSERT ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER project_repos_delete AFTER DELETE ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
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
    "ALTER TABLE tasks ADD COLUMN human INTEGER NOT NULL DEFAULT 0;",
    // Tokens say who calls, they are not passwords: they are kept as they are, so one can be handed out again.
    "CREATE TABLE accounts (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
         kind       TEXT NOT NULL,
         token      TEXT NOT NULL UNIQUE,
         project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     ALTER TABLE tasks ADD COLUMN assignee INTEGER REFERENCES accounts(id) ON DELETE SET NULL;
     CREATE INDEX tasks_by_assignee ON tasks(assignee);
     CREATE TRIGGER accounts_insert AFTER INSERT ON accounts BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER accounts_delete AFTER DELETE ON accounts BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // A project's own columns, each placed right after another one.
    "CREATE TABLE statuses (
         id          INTEGER PRIMARY KEY AUTOINCREMENT,
         project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
         name        TEXT NOT NULL,
         description TEXT NOT NULL,
         after       TEXT NOT NULL,
         UNIQUE (project_id, name)
     );
     CREATE TRIGGER statuses_insert AFTER INSERT ON statuses BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER statuses_update AFTER UPDATE ON statuses BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER statuses_delete AFTER DELETE ON statuses BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // `agents` marks a task as AI agents' work: queued for them while it has no assignee.
    "ALTER TABLE tasks ADD COLUMN agents INTEGER NOT NULL DEFAULT 0;
     UPDATE tasks SET agents = 1 WHERE assignee IN (SELECT id FROM accounts WHERE kind <> 'human');
     ALTER TABLE statuses ADD COLUMN icon TEXT;",
    // A comment with these set notes a status change; its body says so for older Kanzos.
    "ALTER TABLE comments ADD COLUMN moved_from TEXT;
     ALTER TABLE comments ADD COLUMN moved_to TEXT;",
    // Notes of assignment changes; "" is nobody.
    "ALTER TABLE comments ADD COLUMN assigned_from TEXT;
     ALTER TABLE comments ADD COLUMN assigned_to TEXT;",
    // A template is a pattern for new projects: its tasks are never handed to agents.
    "ALTER TABLE projects ADD COLUMN template INTEGER NOT NULL DEFAULT 0;",
    // A subtask's parent; deleting the parent leaves its subtasks as tasks of their own.
    "ALTER TABLE tasks ADD COLUMN parent INTEGER REFERENCES tasks(id) ON DELETE SET NULL;
     CREATE INDEX tasks_by_parent ON tasks(parent);",
    // Upstream's three come after the accounts..parent ones above: databases already
    // applied those as versions 7 to 13, so these simply apply next.
    "ALTER TABLE tasks ADD COLUMN url TEXT NOT NULL DEFAULT '';",
    "CREATE TABLE task_waits (
         task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         waits_on_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         PRIMARY KEY (task_id, waits_on_id)
     ) WITHOUT ROWID;
     CREATE INDEX task_waits_by_blocker ON task_waits(waits_on_id);
     CREATE TRIGGER task_waits_insert AFTER INSERT ON task_waits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER task_waits_delete AFTER DELETE ON task_waits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    "ALTER TABLE projects ADD COLUMN position REAL;",
];

const PROJECT_COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
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
    pub repos: Vec<String>,
    pub color: Option<String>,
    pub created_at: String,
    pub open: i64,
    pub total: i64,
    /// A pattern for new projects; agents don't take its tasks.
    pub template: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// A link to anything, like the task's pull request; empty when there is none.
    pub url: String,
    pub human: bool,
    pub status: String,
    pub position: f64,
    pub created_at: String,
    pub updated_at: String,
    pub comments: i64,
    /// The name of the account it is assigned to.
    pub assignee: Option<String>,
    /// The parent task's key, like api#12, when this is a subtask.
    pub parent: Option<String>,
    /// The tasks this one waits on that aren't done yet. Left out when there are none, as on most tasks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<i64>,
}

impl Task {
    pub fn key(&self) -> String {
        format!("{}#{}", self.project, self.id)
    }
}

/// A task another one waits on or blocks.
#[derive(Debug, Clone, Serialize)]
pub struct TaskLink {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// Set on a note of a status change, made by `author`, rather than a comment.
    pub moved_from: Option<String>,
    pub moved_to: Option<String>,
    /// Set on a note of an assignment change; "" is nobody.
    pub assigned_from: Option<String>,
    pub assigned_to: Option<String>,
}

impl Comment {
    /// A note of a move or an assignment, rather than something someone said.
    pub fn is_note(&self) -> bool {
        self.moved_to.is_some() || self.assigned_to.is_some()
    }
}

/// Who an account is. Humans can do anything; the others are limited in `mcp.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Human,
    Orchestrator,
    Agent,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Human => "human",
            Kind::Orchestrator => "orchestrator",
            Kind::Agent => "agent",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Human => "human",
            Kind::Orchestrator => "orchestrator",
            Kind::Agent => "task agent",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        match s.trim().to_ascii_lowercase().replace(['-', '_'], " ").as_str() {
            "human" => Ok(Kind::Human),
            "orchestrator" => Ok(Kind::Orchestrator),
            "agent" | "task agent" => Ok(Kind::Agent),
            other => Err(Error::Invalid(format!("unknown account kind {other:?}; use human, orchestrator or agent"))),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    pub kind: Kind,
    /// Slug of the project an orchestrator or agent works for; `None` for all of them.
    pub project: Option<String>,
    pub token: String,
    pub created_at: String,
}

/// A column a person added to one project, with what it means for agents.
#[derive(Debug, Clone, Serialize)]
pub struct CustomStatus {
    pub project: String,
    pub name: String,
    pub description: String,
    /// The column it comes right after.
    pub after: String,
    /// One of `STATUS_ICONS`; `None` for the default look.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub id: i64,
    pub task_id: i64,
    pub comment_id: Option<i64>,
    pub file: String,
}

#[derive(Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub project_id: Option<i64>,
    pub tags: Option<String>,
    pub url: Option<String>,
    pub human: Option<bool>,
    /// Task keys, comma-separated; it replaces the list.
    pub waits_on: Option<String>,
    pub assignee: Option<Assignee>,
    /// `Some(None)` makes it a task of its own again.
    pub parent: Option<Option<i64>>,
}

/// Who a task is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignee {
    Nobody,
    /// Queued for AI agents, until an orchestrator hands it to one.
    Agents,
    Account(i64),
}

/// How a task queued for AI agents shows its assignee, as `TASK_SELECT` spells it. No account can take this name.
pub const AI_AGENTS: &str = "AI Agents";

/// The board's own name for its user, on comments and moves.
pub const BOARD_USER: &str = "me";

pub const STATUS_ICONS: [&str; 12] =
    ["dot", "eye", "flag", "star", "check", "bug", "rocket", "clock", "person", "chat", "shield", "beaker"];

fn clean_icon(icon: Option<&str>) -> Result<Option<String>> {
    match icon.map(|i| i.trim().to_ascii_lowercase()).filter(|i| !i.is_empty()) {
        None => Ok(None),
        Some(i) if STATUS_ICONS.contains(&i.as_str()) => Ok(Some(i)),
        Some(i) => Err(Error::Invalid(format!("unknown icon {i:?}; use one of: {}", STATUS_ICONS.join(", ")))),
    }
}

const STATUSES: [&str; 5] = ["todo", "doing", "blocked", "review", "done"];

pub const DOING: &str = STATUSES[1];
pub const BLOCKED: &str = STATUSES[2];
pub const REVIEW: &str = STATUSES[3];
pub const DONE: &str = STATUSES[4];

pub const BACKLOG: &str = "backlog";

/// What a task shows besides its id, title and status; each can be turned off in Settings.
const PROPERTIES: [&str; 9] =
    ["description", "tags", "url", "human", "waiting", "created", "updated", "comments", "images"];

pub struct Store {
    conn: Connection,
    statuses: Vec<String>,
    /// Declared after `conn` so it is released after the connection closes.
    _lock: Option<std::fs::File>,
    /// Who status changes are noted as made by.
    actor: std::cell::RefCell<String>,
}

impl Store {
    pub fn open(path: &Path, lock: Option<std::fs::File>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // The board and any number of agents share this file; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Per connection, not stored in the file. Also turns a WAL database from an
        // older kanzo back into a rollback-journal one once nothing else has it open.
        // `synchronous` stays at SQLite's default FULL, which keeps the file intact
        // when a board loses power.
        conn.pragma_update_and_check(None, "journal_mode", "TRUNCATE", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Before the first migration there is no meta table, and so no backlog.
        let backlog = conn
            .query_row("SELECT value FROM meta WHERE key = 'backlog'", [], |r| r.get::<_, String>(0))
            .is_ok_and(|v| v == "1");
        let statuses = backlog.then_some(BACKLOG).into_iter().chain(STATUSES).map(String::from).collect();
        Ok(Self { conn, statuses, _lock: lock, actor: std::cell::RefCell::new(BOARD_USER.into()) })
    }

    /// Notes the status changes made from now on as `actor`'s.
    pub fn acting_as(&self, actor: &str) {
        *self.actor.borrow_mut() = actor.to_string();
    }

    /// Appends "moved from <from> to <to> by <actor>" to the task's comments.
    fn note_move(&self, c: &Connection, task_id: i64, from: &str, to: &str) -> Result<()> {
        let actor = self.actor.borrow();
        c.execute(
            "INSERT INTO comments (task_id, author, body, moved_from, moved_to) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, *actor, format!("moved from {from} to {to}"), from, to],
        )?;
        Ok(())
    }

    /// Appends "assigned to <to>", "reassigned from <from> to <to>" or "unassigned from <from>",
    /// by the actor, when the task's assignee is no longer `before`.
    fn note_assignment(&self, c: &Connection, task_id: i64, before: Option<&str>) -> Result<()> {
        let after = self.get_task(task_id)?.assignee;
        if after.as_deref() == before {
            return Ok(());
        }
        let (from, to) = (before.unwrap_or(""), after.as_deref().unwrap_or(""));
        let body = match (from, to) {
            ("", to) => format!("assigned to {to}"),
            (from, "") => format!("unassigned from {from}"),
            (from, to) => format!("reassigned from {from} to {to}"),
        };
        let actor = self.actor.borrow();
        c.execute(
            "INSERT INTO comments (task_id, author, body, assigned_from, assigned_to) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, *actor, body, from, to],
        )?;
        Ok(())
    }

    /// A project's columns in board order, or with `None`, every project's.
    pub fn statuses_for(&self, project_id: Option<i64>) -> Result<Vec<String>> {
        let mut out = self.statuses.clone();
        // Columns placed after the same one keep the order they were added in.
        let mut last_after: Vec<(String, String)> = Vec::new();
        for c in self.custom_statuses(project_id)? {
            if out.contains(&c.name) {
                continue;
            }
            let anchor = last_after.iter().find(|(a, _)| *a == c.after).map_or(&c.after, |(_, last)| last);
            // Its column may be gone, like backlog turned off: it then goes just before done.
            let at = out.iter().position(|s| s == anchor).map_or(out.len() - 1, |i| i + 1);
            out.insert(at, c.name.clone());
            last_after.retain(|(a, _)| *a != c.after);
            last_after.push((c.after, c.name));
        }
        Ok(out)
    }

    pub fn custom_statuses(&self, project_id: Option<i64>) -> Result<Vec<CustomStatus>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.slug, s.name, s.description, s.after, s.icon FROM statuses s JOIN projects p ON p.id = s.project_id
             WHERE ?1 IS NULL OR s.project_id = ?1 ORDER BY s.id",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            Ok(CustomStatus {
                project: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                after: r.get(3)?,
                icon: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a column to the project, after `after` or else just before done. Its description
    /// tells agents what it is for.
    pub fn add_status(
        &self,
        project_id: i64,
        name: &str,
        description: &str,
        after: Option<&str>,
        icon: Option<&str>,
    ) -> Result<CustomStatus> {
        let name = squash(name).to_lowercase();
        let ok = |c: char| c.is_alphanumeric() || c == ' ' || c == '-';
        if name.is_empty() || name.chars().count() > 24 || !name.chars().all(ok) {
            return Err(Error::Invalid("a status name needs 1 to 24 letters, digits, spaces or -".into()));
        }
        let description = description.trim();
        if description.is_empty() {
            return Err(Error::Invalid("describe the status, so agents know what it means".into()));
        }
        let icon = clean_icon(icon)?;
        let after = after.map(|a| squash(a).to_lowercase()).filter(|a| !a.is_empty()).unwrap_or(REVIEW.into());
        self.write(|c| {
            let columns = self.statuses_for(Some(project_id))?;
            if columns.contains(&name) || name == BACKLOG || STATUSES.contains(&name.as_str()) {
                return Err(Error::Invalid(format!("there is already a status {name:?}")));
            }
            if after == DONE || !columns.contains(&after) {
                let before_done = &columns[..columns.len() - 1];
                return Err(Error::Invalid(format!(
                    "can't place it after {after:?}; use one of: {}",
                    before_done.join(", ")
                )));
            }
            c.execute(
                "INSERT INTO statuses (project_id, name, description, after, icon) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project_id, name, description, after, icon],
            )?;
            Ok(())
        })?;
        let added = self.custom_statuses(Some(project_id))?.into_iter().find(|s| s.name == name);
        added.ok_or_else(|| Error::NotFound(format!("no status {name:?}")))
    }

    /// Changes a column's icon; `None` or "" goes back to the default look.
    pub fn set_status_icon(&self, project_id: i64, name: &str, icon: Option<&str>) -> Result<CustomStatus> {
        let name = squash(name).to_lowercase();
        let icon = clean_icon(icon)?;
        self.conn.execute(
            "UPDATE statuses SET icon = ?1 WHERE project_id = ?2 AND name = ?3 AND icon IS NOT ?1",
            params![icon, project_id, name],
        )?;
        let status = self.custom_statuses(Some(project_id))?.into_iter().find(|s| s.name == name);
        status.ok_or_else(|| Error::NotFound(format!("the project has no status {name:?} of its own")))
    }

    /// Only an empty column can go; the ones placed after it take its place.
    pub fn remove_status(&self, project_id: i64, name: &str) -> Result<CustomStatus> {
        let name = squash(name).to_lowercase();
        let status = self
            .custom_statuses(Some(project_id))?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| Error::NotFound(format!("the project has no status {name:?} of its own")))?;
        self.write(|c| {
            let left: i64 = c.query_row(
                "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status = ?2",
                params![project_id, name],
                |r| r.get(0),
            )?;
            if left > 0 {
                return Err(Error::Invalid(format!("move the {left} {} out of {name} first", tasks(left))));
            }
            c.execute(
                "UPDATE statuses SET after = ?1 WHERE project_id = ?2 AND after = ?3",
                params![status.after, project_id, name],
            )?;
            c.execute("DELETE FROM statuses WHERE project_id = ?1 AND name = ?2", params![project_id, name])?;
            Ok(())
        })?;
        Ok(status)
    }

    pub fn set_backlog(&self, on: bool) -> Result<()> {
        self.write(|c| {
            let left: i64 = c.query_row("SELECT count(*) FROM tasks WHERE status = ?1", [BACKLOG], |r| r.get(0))?;
            if !on && left > 0 {
                return Err(Error::Invalid(format!("move the {left} {} out of backlog first", tasks(left))));
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

    /// The properties turned off in Settings: off the board, and out of what agents get.
    pub fn hidden_properties(&self) -> Result<Vec<String>> {
        let hidden: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'hidden_properties'", [], |r| r.get(0))
            .optional()?;
        Ok(hidden.unwrap_or_default().split(',').filter(|p| !p.is_empty()).map(String::from).collect())
    }

    pub fn set_property(&self, property: &str, visible: bool) -> Result<()> {
        if !PROPERTIES.contains(&property) {
            return Err(Error::Invalid(format!("unknown property {property:?}")));
        }
        self.write(|c| {
            let mut hidden = self.hidden_properties()?;
            hidden.retain(|p| p != property);
            if !visible {
                hidden.push(property.to_string());
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('hidden_properties', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [hidden.join(",")],
            )?;
            // Other open boards redraw their tasks.
            c.execute("UPDATE meta SET value = value + 1 WHERE key = 'changes'", [])?;
            Ok(())
        })
    }

    /// Immediate, so concurrent writers queue up on the busy timeout instead of failing halfway.
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

    /// Writes nothing when up to date, so starting `kanzo mcp` for every agent session
    /// costs no disk write.
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
                // From a newer Kanzo, maybe synced from another machine: use it as it is
                // when this version can, without touching its schema.
                if meta(c, "compatible")?.is_some_and(|oldest| oldest <= latest) {
                    return Ok(());
                }
                return Err(Error::Invalid(
                    "this board was saved by a newer version of Kanzo; update this one with `kanzo update`".into(),
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

    pub fn changes(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'changes'")?
            .query_row([], |r| r.get(0))?)
    }

    pub fn now(&self) -> Result<String> {
        Ok(self.conn.query_row(&format!("SELECT {NOW}"), [], |r| r.get(0))?)
    }

    /// Makes `kanzo.db` complete on its own: rolls back a write that a crashed process
    /// left half done, and folds in a WAL left by an older kanzo.
    pub fn settle(&self) -> Result<()> {
        self.conn.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    const PROJECT_SELECT: &str = "SELECT p.id, p.slug, p.name,
            (SELECT group_concat(repo, ',' ORDER BY r.rowid) FROM project_repos r WHERE r.project_id = p.id),
            p.color, p.created_at,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.status <> ?1),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id),
            p.template
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
            template: r.get(8)?,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let sql = format!("{} ORDER BY p.position IS NULL, p.position, p.slug", Self::PROJECT_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([DONE], Self::project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn project_where(&self, clause: &str, value: &str) -> Result<Option<Project>> {
        let sql = format!("{} WHERE {clause}", Self::PROJECT_SELECT);
        Ok(self.conn.query_row(&sql, params![DONE, value], Self::project_row).optional()?)
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
        // Checked in the write's transaction, so nobody can take the slug or the repo in between.
        self.write(|c| {
            if self.project_by_slug(&slug)?.is_some() {
                return Err(Error::Invalid(format!("project {slug} already exists")));
            }
            if let Some(r) = &repo {
                self.check_repo_free(r)?;
            }
            c.execute("INSERT INTO projects (slug, name) VALUES (?1, ?2)", params![slug, name])?;
            if let Some(r) = &repo {
                c.execute(
                    "INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)",
                    params![r, c.last_insert_rowid()],
                )?;
            }
            Ok(())
        })?;
        self.find_project(&slug)
    }

    pub fn link_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = clean_repo(repo)?;
        if project.repos.contains(&repo) {
            return Ok(project);
        }
        self.write(|c| {
            self.check_repo_free(&repo)?;
            c.execute("INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)", params![repo, project.id])?;
            Ok(())
        })?;
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

    pub fn rename_project(&self, slug: &str, name: &str) -> Result<Project> {
        let name = squash(name);
        if name.is_empty() {
            return Err(Error::Invalid("project name is empty".into()));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET name = ?1 WHERE id = ?2", params![name, project.id])?;
        self.find_project(&project.slug)
    }

    pub fn set_project_color(&self, slug: &str, color: &str) -> Result<Project> {
        if !PROJECT_COLORS.contains(&color) {
            return Err(Error::Invalid(format!("unknown color {color:?}; use one of: {}", PROJECT_COLORS.join(", "))));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET color = ?1 WHERE id = ?2", params![color, project.id])?;
        self.find_project(&project.slug)
    }

    /// Before `before`, or last. A project never moved has no position and comes after those that
    /// have one, by slug, so only a move that lands among those has to number them all.
    pub fn move_project(&self, slug: &str, before: Option<&str>) -> Result<Project> {
        let project = self.find_project(slug)?;
        let before = before.map(|b| self.find_project(b)).transpose()?;
        if before.as_ref().is_some_and(|b| b.id == project.id) {
            return Ok(project);
        }
        self.write(|c| {
            let mut others: Vec<(i64, Option<f64>)> = c
                .prepare("SELECT id, position FROM projects WHERE id <> ?1 ORDER BY position IS NULL, position, slug")?
                .query_map([project.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let at = before.and_then(|b| others.iter().position(|o| o.0 == b.id)).unwrap_or(others.len());
            let position = match (at.checked_sub(1).map(|i| others[i].1), others.get(at).map(|o| o.1)) {
                (None, Some(Some(next))) => Some(next - 1.0),
                (None, _) => Some(1.0),
                (Some(Some(prev)), Some(Some(next))) if next - prev >= 1e-9 => Some((prev + next) / 2.0),
                (Some(Some(prev)), Some(None) | None) => Some(prev + 1.0),
                _ => None,
            };
            let mut update = c.prepare("UPDATE projects SET position = ?1 WHERE id = ?2")?;
            if let Some(position) = position {
                update.execute(params![position, project.id])?;
            } else {
                others.insert(at, (project.id, None));
                for (i, (id, _)) in others.iter().enumerate() {
                    update.execute(params![(i + 1) as f64, id])?;
                }
            }
            Ok(())
        })?;
        self.find_project(&project.slug)
    }

    pub fn set_project_template(&self, slug: &str, on: bool) -> Result<Project> {
        let project = self.find_project(slug)?;
        self.conn
            .execute("UPDATE projects SET template = ?1 WHERE id = ?2 AND template <> ?1", params![on, project.id])?;
        self.find_project(&project.slug)
    }

    /// A new project starting as a copy of a template: its own columns, and its tasks that
    /// aren't done, with their tags, Human flags, task images, and AI Agents or human
    /// assignees. Comments, repos and agents' own assignments stay with the template.
    pub fn create_project_from(&self, slug: &str, name: &str, repo: Option<&str>, template: &str) -> Result<Project> {
        let template = self.find_project(template)?;
        if !template.template {
            return Err(Error::Invalid(format!("{} isn't a template; mark it as one first", template.slug)));
        }
        let project = self.create_project(slug, name, repo)?;
        if let Err(e) = self.copy_template(template.id, project.id) {
            let _ = self.delete_project(&project.slug);
            return Err(e);
        }
        self.find_project(&project.slug)
    }

    fn copy_template(&self, from: i64, to: i64) -> Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT INTO statuses (project_id, name, description, after, icon)
                 SELECT ?2, name, description, after, icon FROM statuses WHERE project_id = ?1 ORDER BY id",
                params![from, to],
            )?;
            let tasks: Vec<(i64, String, Option<i64>)> = c
                .prepare("SELECT id, status, parent FROM tasks WHERE project_id = ?1 AND status <> ?2 ORDER BY position")?
                .query_map(params![from, DONE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut copies = std::collections::HashMap::new();
            for (id, status, _) in &tasks {
                // Taken in board order, so each column keeps the template's order.
                let position = Self::end_of_column(c, status, 0)?;
                c.execute(
                    "INSERT INTO tasks (project_id, title, description, status, position, tags, human, agents, assignee)
                     SELECT ?1, title, description, status, ?2, tags, human, agents,
                       CASE WHEN assignee IN (SELECT id FROM accounts WHERE kind = 'human') THEN assignee END
                     FROM tasks WHERE id = ?3",
                    params![to, position, id],
                )?;
                let copy = c.last_insert_rowid();
                copies.insert(*id, copy);
                c.execute(
                    "INSERT INTO images (task_id, file) SELECT ?1, file FROM images WHERE task_id = ?2 AND comment_id IS NULL",
                    params![copy, id],
                )?;
            }
            // Subtasks stay under the copy of their parent.
            for (id, _, parent) in &tasks {
                if let Some(parent) = parent.and_then(|p| copies.get(&p)) {
                    c.execute("UPDATE tasks SET parent = ?1 WHERE id = ?2", params![parent, copies[id]])?;
                }
            }
            Ok(())
        })
    }
    pub fn delete_project(&self, slug: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        // Tasks and comments go with it through ON DELETE CASCADE.
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [project.id])?;
        Ok(project)
    }

    const TASK_SELECT: &str = "SELECT t.id, p.slug, t.title, t.description, t.status, t.position,
            t.created_at, t.updated_at,
            (SELECT count(*) FROM comments c WHERE c.task_id = t.id AND c.moved_to IS NULL AND c.assigned_to IS NULL),
            t.tags, t.human, t.url,
            coalesce((SELECT name FROM accounts a WHERE a.id = t.assignee), CASE WHEN t.agents THEN 'AI Agents' END),
            (SELECT pp.slug || '#' || tp.id FROM tasks tp JOIN projects pp ON pp.id = tp.project_id WHERE tp.id = t.parent)
        FROM tasks t JOIN projects p ON p.id = t.project_id";

    fn task_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            tags: r.get::<_, String>(9)?.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
            url: r.get(11)?,
            human: r.get(10)?,
            status: r.get(4)?,
            position: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            comments: r.get(8)?,
            waiting_on: Vec::new(),
            assignee: r.get(12)?,
            parent: r.get(13)?,
        })
    }

    /// One query for all of them, not a subquery per task: on a board of 3,000 tasks that took
    /// /api/board from 5.3 ms to 7.3 ms even with nothing waiting. Now tasks pay only for actual links.
    fn add_waiting(&self, tasks: &mut [Task]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT w.task_id, w.waits_on_id FROM task_waits w JOIN tasks b ON b.id = w.waits_on_id
             WHERE b.status <> 'done' ORDER BY w.waits_on_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut waiting: HashMap<i64, Vec<i64>> = HashMap::new();
        while let Some(r) = rows.next()? {
            waiting.entry(r.get(0)?).or_default().push(r.get(1)?);
        }
        if !waiting.is_empty() {
            for t in tasks {
                t.waiting_on = waiting.remove(&t.id).unwrap_or_default();
            }
        }
        Ok(())
    }

    pub fn list_tasks(&self, project_id: Option<i64>, status: Option<&str>) -> Result<Vec<Task>> {
        Ok(self.list_tasks_keeping_done(project_id, status, None, None)?.0)
    }

    /// The done column only grows, so only its newest `keep_done` tasks are read.
    /// `assignee` keeps only the tasks of that account.
    pub fn list_tasks_keeping_done(
        &self,
        project_id: Option<i64>,
        status: Option<&str>,
        assignee: Option<i64>,
        keep_done: Option<usize>,
    ) -> Result<(Vec<Task>, usize)> {
        let done = DONE;
        // SQLite reads a negative LIMIT as no limit.
        let limit = keep_done.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE (?1 IS NULL OR t.project_id = ?1) AND (?2 IS NULL OR t.status = ?2)
               AND (?5 IS NULL OR t.assignee = ?5)
               AND (t.status <> ?3 OR t.id IN (SELECT id FROM tasks WHERE status = ?3
                    AND (?1 IS NULL OR project_id = ?1) AND (?5 IS NULL OR assignee = ?5)
                    ORDER BY position DESC LIMIT ?4))
             ORDER BY t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks: Vec<Task> = stmt
            .query_map(params![project_id, status, done, limit, assignee], Self::task_row)?
            .collect::<rusqlite::Result<_>>()?;
        self.add_waiting(&mut tasks)?;
        let columns = self.statuses_for(project_id)?;
        let column = |t: &Task| columns.iter().position(|s| *s == t.status).unwrap_or(usize::MAX);
        tasks.sort_by_key(column);
        let hidden = match keep_done {
            Some(keep) if status.is_none_or(|s| s == done) => {
                let all: i64 = self.conn.query_row(
                    "SELECT count(*) FROM tasks WHERE status = ?1 AND (?2 IS NULL OR project_id = ?2)
                       AND (?3 IS NULL OR assignee = ?3)",
                    params![done, project_id, assignee],
                    |r| r.get(0),
                )?;
                usize::try_from(all).unwrap_or(0).saturating_sub(keep)
            }
            _ => 0,
        };
        Ok((tasks, hidden))
    }

    // SQLite's LIKE ignores case only for ASCII. Registered per query, not in open, because
    // every operation opens a connection and only filters need this.
    fn add_has_word(&self) -> Result<()> {
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        self.conn.create_scalar_function("has_word", 2, flags, |c| {
            Ok(c.get_raw(0).as_str()?.to_lowercase().contains(c.get_raw(1).as_str()?))
        })?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<Vec<Task>> {
        self.search_seeing(query, limit, &[])
    }

    /// Finds nothing through a property Settings hides from agents.
    pub fn search_for_agent(&self, query: &str) -> Result<Vec<Task>> {
        self.search_seeing(query, None, &self.hidden_properties()?)
    }

    fn search_seeing(&self, query: &str, limit: Option<usize>, hidden: &[String]) -> Result<Vec<Task>> {
        self.add_has_word()?;
        let filter = Filter::new(query, 2, hidden);
        // SQLite reads a negative LIMIT as no limit.
        let limit = limit.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE 1{} ORDER BY t.status = ?1, t.updated_at DESC, t.id DESC LIMIT {limit}",
            Self::TASK_SELECT,
            filter.sql
        );
        let mut values: Vec<String> = vec![DONE.to_string()];
        values.extend(filter.values);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), Self::task_row)?;
        let mut tasks: Vec<Task> = rows.collect::<rusqlite::Result<_>>()?;
        self.add_waiting(&mut tasks)?;
        Ok(tasks)
    }

    pub fn get_task(&self, id: i64) -> Result<Task> {
        let sql = format!("{} WHERE t.id = ?1", Self::TASK_SELECT);
        let mut task = self
            .conn
            .prepare_cached(&sql)?
            .query_row([id], Self::task_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no task #{id}")))?;
        task.waiting_on = self.task_links(id, false)?.into_iter().filter(|t| t.status != DONE).map(|t| t.id).collect();
        Ok(task)
    }

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

    /// Whether the project has the column; with `None`, whether any project has it.
    pub fn check_status(&self, project_id: Option<i64>, status: &str) -> Result<()> {
        let statuses = self.statuses_for(project_id)?;
        if statuses.iter().any(|s| s == status) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("unknown status {status:?}; use one of: {}", statuses.join(", "))))
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

    #[cfg(test)]
    pub fn create_task(&self, project: i64, title: &str, description: &str) -> Result<Task> {
        self.create_task_in(project, title, TaskPatch { description: Some(description.into()), ..Default::default() })
    }

    // One write, or an agent could take a task before it's human, before it waits, or before
    // it is queued for AI agents. Uses description, status, tags, human, waits_on, parent, assignee.
    pub fn create_task_in(&self, project_id: i64, title: &str, patch: TaskPatch) -> Result<Task> {
        let title = clean_title(title)?;
        let status = patch.status.as_deref().unwrap_or(STATUSES[0]);
        self.check_status(Some(project_id), status)?;
        let description = patch.description.as_deref().unwrap_or("").trim();
        let tags = clean_tags(patch.tags.as_deref().unwrap_or("")).join(",");
        let id = self.write(|c| {
            let position = Self::end_of_column(c, status, 0)?;
            let (assignee, agents) = match patch.assignee {
                None | Some(Assignee::Nobody) => (None, false),
                Some(Assignee::Agents) => (None, true),
                Some(Assignee::Account(a)) => {
                    (Some(a), c.query_row("SELECT kind <> 'human' FROM accounts WHERE id = ?1", [a], |r| r.get(0))?)
                }
            };
            c.execute(
                "INSERT INTO tasks (project_id, title, description, status, position, tags, human, assignee, agents) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    project_id,
                    title,
                    description,
                    status,
                    position,
                    tags,
                    patch.human.unwrap_or(false),
                    assignee,
                    agents
                ],
            )?;
            let id = c.last_insert_rowid();
            if let Some(Some(parent)) = patch.parent {
                Self::check_parent(c, id, parent)?;
                c.execute("UPDATE tasks SET parent = ?1 WHERE id = ?2", params![parent, id])?;
            }
            if let Some(keys) = &patch.waits_on {
                self.set_waits(c, id, keys)?;
            }
            Ok(id)
        })?;
        self.get_task(id)
    }

    /// Replaces what a task waits on. Part of the task's write, so a refused list changes nothing.
    fn set_waits(&self, c: &Connection, id: i64, keys: &str) -> Result<()> {
        let mut ids = Vec::new();
        for key in keys.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            ids.push(self.find_task(key)?.id);
        }
        ids.sort_unstable();
        ids.dedup();
        if ids.iter().copied().eq(self.task_links(id, false)?.iter().map(|t| t.id)) {
            return Ok(());
        }
        c.execute("DELETE FROM task_waits WHERE task_id = ?1", [id])?;
        for other in ids {
            let loops: bool = c.query_row(
                "WITH RECURSIVE chain(id) AS (
                     SELECT ?1 UNION SELECT w.waits_on_id FROM task_waits w JOIN chain ON w.task_id = chain.id
                 ) SELECT EXISTS (SELECT 1 FROM chain WHERE id = ?2)",
                [other, id],
                |r| r.get(0),
            )?;
            if loops {
                return Err(Error::Invalid(format!("#{id} can't wait on #{other}: that would make a loop")));
            }
            c.execute("INSERT INTO task_waits (task_id, waits_on_id) VALUES (?1, ?2)", [id, other])?;
        }
        c.execute(&format!("UPDATE tasks SET updated_at = {NOW} WHERE id = ?1"), [id])?;
        Ok(())
    }

    /// What a task waits on, done or not, or with `blocks`, the tasks waiting on it.
    pub fn task_links(&self, id: i64, blocks: bool) -> Result<Vec<TaskLink>> {
        let (this, other) = if blocks { ("waits_on_id", "task_id") } else { ("task_id", "waits_on_id") };
        let sql = format!(
            "SELECT t.id, p.slug, t.title, t.status FROM task_waits w JOIN tasks t ON t.id = w.{other}
             JOIN projects p ON p.id = t.project_id WHERE w.{this} = ?1 ORDER BY t.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([id], |r| {
            Ok(TaskLink { id: r.get(0)?, project: r.get(1)?, title: r.get(2)?, status: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<Task> {
        let title = patch.title.as_deref().map(clean_title).transpose()?;
        let url = patch.url.as_deref().map(clean_url).transpose()?;
        self.write(|c| {
            let current = self.get_task(id)?;
            // The column has to exist in the project the task ends up in.
            if patch.status.is_some() || patch.project_id.is_some() {
                let project_id = match patch.project_id {
                    Some(p) => p,
                    None => c.query_row("SELECT project_id FROM tasks WHERE id = ?1", [id], |r| r.get(0))?,
                };
                self.check_status(Some(project_id), patch.status.as_deref().unwrap_or(&current.status))?;
            }
            // Only what the patch changes is written, and only then is the task newer.
            let tags = patch.tags.as_deref().map(|t| clean_tags(t).join(","));
            c.execute(
                &format!(
                    "UPDATE tasks SET title = coalesce(?1, title), project_id = coalesce(?2, project_id),
                       human = coalesce(?3, human), tags = coalesce(?4, tags),
                       description = coalesce(?5, description), url = coalesce(?7, url), updated_at = {NOW}
                     WHERE id = ?6 AND (coalesce(?1, title), coalesce(?2, project_id), coalesce(?3, human),
                       coalesce(?4, tags), coalesce(?5, description), coalesce(?7, url))
                       <> (title, project_id, human, tags, description, url)"
                ),
                params![
                    title,
                    patch.project_id,
                    patch.human,
                    tags,
                    patch.description.as_deref().map(str::trim),
                    id,
                    url
                ],
            )?;
            if let Some(parent) = patch.parent {
                if let Some(parent) = parent {
                    Self::check_parent(c, id, parent)?;
                }
                c.execute(
                    &format!("UPDATE tasks SET parent = ?1, updated_at = {NOW} WHERE id = ?2 AND parent IS NOT ?1"),
                    params![parent, id],
                )?;
            }
            if let Some(assignee) = patch.assignee {
                // A task given to an agent or orchestrator stays AI agents' work.
                let (account, agents) = match assignee {
                    Assignee::Nobody => (None, "0"),
                    Assignee::Agents => (None, "1"),
                    Assignee::Account(id) => (Some(id), "(SELECT kind <> 'human' FROM accounts WHERE id = ?1)"),
                };
                c.execute(
                    &format!(
                        "UPDATE tasks SET assignee = ?1, agents = {agents}, updated_at = {NOW}
                         WHERE id = ?2 AND (assignee IS NOT ?1 OR agents IS NOT {agents})"
                    ),
                    params![account, id],
                )?;
                self.note_assignment(c, id, current.assignee.as_deref())?;
            }
            if let Some(s) = &patch.status
                && *s != current.status
            {
                let position = Self::end_of_column(c, s, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![s, position, id],
                )?;
                self.note_move(c, id, &current.status, s)?;
            }
            if let Some(keys) = &patch.waits_on {
                self.set_waits(c, id, keys)?;
            }
            Ok(())
        })?;
        self.get_task(id)
    }

    /// In one write: all or nothing, and about 60 times faster than one write per task. Tags are
    /// added and removed, not replaced, so each task keeps its others.
    pub fn update_tasks(&self, ids: &[i64], status: Option<&str>, add_tags: &str, remove_tags: &str) -> Result<()> {
        let (add, remove) = (clean_tags(add_tags), clean_tags(remove_tags));
        self.write(|c| {
            for &id in ids {
                let task = self.get_task(id)?;
                let mut tags = task.tags.clone();
                tags.retain(|t| !remove.contains(t));
                for tag in &add {
                    if !tags.contains(tag) {
                        tags.push(tag.clone());
                    }
                }
                if tags != task.tags {
                    c.execute(
                        &format!("UPDATE tasks SET tags = ?1, updated_at = {NOW} WHERE id = ?2"),
                        params![tags.join(","), id],
                    )?;
                }
                if let Some(s) = status.filter(|s| *s != task.status) {
                    // Each task's own project has to have the column.
                    let project_id: i64 =
                        c.query_row("SELECT project_id FROM tasks WHERE id = ?1", [id], |r| r.get(0))?;
                    self.check_status(Some(project_id), s)?;
                    let position = Self::end_of_column(c, s, id)?;
                    c.execute(
                        &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                        params![s, position, id],
                    )?;
                    self.note_move(c, id, &task.status, s)?;
                }
            }
            Ok(())
        })
    }

    pub fn move_task(&self, id: i64, status: &str, before: Option<i64>) -> Result<Task> {
        self.write(|c| self.place(c, id, status, before))?;
        self.get_task(id)
    }

    /// In one write, so nobody sees the group half moved. They keep the order of `ids`.
    pub fn move_tasks(&self, ids: &[i64], status: &str, before: Option<i64>) -> Result<()> {
        let before = before.filter(|b| !ids.contains(b));
        self.write(|c| ids.iter().try_for_each(|&id| self.place(c, id, status, before)))
    }

    fn place(&self, c: &Connection, id: i64, status: &str, before: Option<i64>) -> Result<()> {
        let current = self.get_task(id)?;
        // The column has to exist in the task's own project.
        let project_id: i64 = c.query_row("SELECT project_id FROM tasks WHERE id = ?1", [id], |r| r.get(0))?;
        self.check_status(Some(project_id), status)?;
        let before = before.filter(|b| *b != id);
        let anchor: Option<(i64, f64)> = match before {
            Some(b) => c
                .query_row("SELECT position FROM tasks WHERE id = ?1 AND status = ?2", params![b, status], |r| r.get(0))
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
                    // Out of float room between two neighbours.
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
        if current.status != status {
            self.note_move(c, id, &current.status, status)?;
        }
        Ok(())
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

    #[cfg(test)]
    pub fn next_task(&self, project_id: Option<i64>, assignee: Option<i64>) -> Result<Option<Task>> {
        self.next_task_matching(project_id, assignee, "")
    }

    /// The todo tasks for agents that match `query`, from FROM on, with its values. With
    /// `assignee`, that account's tasks; without, only unassigned ones, so nobody takes a task
    /// meant for an agent. Never a template's.
    fn agents_todo(&self, project_id: Option<i64>, assignee: Option<i64>, query: &str) -> Result<(String, Vec<Value>)> {
        let filter = if query.trim().is_empty() {
            Filter::new("", 4, &[])
        } else {
            self.add_has_word()?;
            Filter::new(query, 4, &self.hidden_properties()?)
        };
        let sql = format!(
            "FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE t.status = ?1 AND t.human = 0 AND (?2 IS NULL OR t.project_id = ?2)
               AND t.assignee IS ?3 AND p.template = 0{}",
            filter.sql
        );
        let mut values: Vec<Value> = vec![STATUSES[0].to_string().into(), project_id.into(), assignee.into()];
        values.extend(filter.values.into_iter().map(Value::from));
        Ok((sql, values))
    }

    /// How many tasks `next_task` skips because they wait on others.
    pub fn waiting_in_todo(&self, project_id: Option<i64>, assignee: Option<i64>, query: &str) -> Result<i64> {
        let (todo, values) = self.agents_todo(project_id, assignee, query)?;
        let sql = format!("SELECT count(*) {todo} AND {WAITING}");
        Ok(self.conn.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))?)
    }

    pub fn next_task_matching(
        &self,
        project_id: Option<i64>,
        assignee: Option<i64>,
        query: &str,
    ) -> Result<Option<Task>> {
        let (todo, values) = self.agents_todo(project_id, assignee, query)?;
        let [from, to, ..] = STATUSES;
        let id = self.write(|c| {
            let sql = format!("SELECT t.id {todo} AND NOT {WAITING} ORDER BY t.position LIMIT 1");
            let id: Option<i64> = c.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0)).optional()?;
            if let Some(id) = id {
                let position = Self::end_of_column(c, to, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![to, position, id],
                )?;
                self.note_move(c, id, from, to)?;
            }
            Ok(id)
        })?;
        id.map(|id| self.get_task(id)).transpose()
    }

    const ACCOUNT_SELECT: &str = "SELECT a.id, a.name, a.kind, p.slug, a.token, a.created_at
        FROM accounts a LEFT JOIN projects p ON p.id = a.project_id";

    fn account_row(r: &Row) -> rusqlite::Result<Account> {
        let kind: String = r.get(2)?;
        Ok(Account {
            id: r.get(0)?,
            name: r.get(1)?,
            // A kind from a newer Kanzo gets the fewest rights.
            kind: Kind::parse(&kind).unwrap_or(Kind::Agent),
            project: r.get(3)?,
            token: r.get(4)?,
            created_at: r.get(5)?,
        })
    }

    fn account_where(&self, clause: &str, value: impl rusqlite::ToSql) -> Result<Option<Account>> {
        let sql = format!("{} WHERE {clause}", Self::ACCOUNT_SELECT);
        Ok(self.conn.query_row(&sql, [value], Self::account_row).optional()?)
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let mut stmt = self.conn.prepare(&format!("{} ORDER BY a.id", Self::ACCOUNT_SELECT))?;
        let rows = stmt.query_map([], Self::account_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Names ignore case, as they do when an account is created.
    pub fn find_account(&self, name: &str) -> Result<Account> {
        self.account_where("a.name = ?1", squash(name))?
            .ok_or_else(|| Error::NotFound(format!("no account named {:?}", squash(name))))
    }

    pub fn account_by_token(&self, token: &str) -> Result<Account> {
        self.account_where("a.token = ?1", token.trim())?
            .ok_or_else(|| Error::NotFound("unknown token: ask the user for yours, or register".into()))
    }

    fn account_by_id(&self, id: i64) -> Result<Account> {
        self.account_where("a.id = ?1", id)?.ok_or_else(|| Error::NotFound(format!("no account {id}")))
    }

    /// Fails when the name is taken, whatever its case: names are how people and agents tell each other apart.
    pub fn create_account(&self, name: &str, kind: Kind, project_id: Option<i64>) -> Result<Account> {
        let name = squash(name);
        if name.is_empty() || name.chars().count() > 64 {
            return Err(Error::Invalid("an account name needs 1 to 64 characters".into()));
        }
        let id = self.write(|c| Self::insert_account(c, &name, kind, project_id))?;
        self.account_by_id(id)
    }

    fn insert_account(c: &Connection, name: &str, kind: Kind, project_id: Option<i64>) -> Result<i64> {
        let taken: bool =
            c.query_row("SELECT EXISTS (SELECT 1 FROM accounts WHERE name = ?1)", [name], |r| r.get(0))?;
        if taken || name.eq_ignore_ascii_case(AI_AGENTS) {
            return Err(Error::Invalid(format!("the name {name:?} is taken")));
        }
        c.execute(
            "INSERT INTO accounts (name, kind, token, project_id)
             VALUES (?1, ?2, 'kz_' || lower(hex(randomblob(16))), ?3)",
            params![name, kind.as_str(), project_id],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn delete_account(&self, name: &str) -> Result<Account> {
        let account = self.find_account(name)?;
        // Its tasks become unassigned through ON DELETE SET NULL.
        self.conn.execute("DELETE FROM accounts WHERE id = ?1", [account.id])?;
        Ok(account)
    }

    /// The top todo task of the project queued for AI agents and not yet handed to one.
    pub fn queued_for_agents(&self, project_id: i64) -> Result<Option<Task>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM tasks WHERE status = ?1 AND human = 0 AND agents = 1 AND assignee IS NULL
                   AND project_id = ?2 AND project_id NOT IN (SELECT id FROM projects WHERE template = 1)
                 ORDER BY position LIMIT 1",
                params![STATUSES[0], project_id],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.get_task(id)).transpose()
    }

    /// Assigns the task to another agent of its project with no work left (nothing outside
    /// review and done), or to a new one named "<project name> Agent <n>". True when the agent is new.
    /// Never to the agent it already has: reassigning is how a blocked task gets fresh help.
    pub fn assign_agent(&self, task_id: i64) -> Result<(Account, bool)> {
        let task = self.get_task(task_id)?;
        let project = self.find_project(&task.project)?;
        let (id, created) = self.write(|c| {
            let before = self.get_task(task_id)?.assignee;
            let idle: Option<i64> = c
                .query_row(
                    "SELECT id FROM accounts a WHERE kind = ?1 AND project_id = ?2
                       AND a.id IS NOT (SELECT assignee FROM tasks WHERE id = ?3)
                       AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.assignee = a.id AND t.status NOT IN (?4, ?5))
                     ORDER BY id LIMIT 1",
                    params![Kind::Agent.as_str(), project.id, task_id, REVIEW, DONE],
                    |r| r.get(0),
                )
                .optional()?;
            let (id, created) = match idle {
                Some(id) => (id, false),
                None => (Self::insert_agent(c, &project)?, true),
            };
            c.execute(
                &format!(
                    "UPDATE tasks SET assignee = ?1, agents = 1, updated_at = {NOW}
                     WHERE id = ?2 AND (assignee IS NOT ?1 OR agents = 0)"
                ),
                params![id, task_id],
            )?;
            self.note_assignment(c, task_id, before.as_deref())?;
            Ok((id, created))
        })?;
        Ok((self.account_by_id(id)?, created))
    }

    /// A new agent for the project, named "<project name> Agent <n>".
    pub fn create_agent(&self, project: &Project) -> Result<Account> {
        let id = self.write(|c| Self::insert_agent(c, project))?;
        self.account_by_id(id)
    }

    fn insert_agent(c: &Connection, project: &Project) -> Result<i64> {
        let count: i64 = c.query_row(
            "SELECT count(*) FROM accounts WHERE kind = ?1 AND project_id = ?2",
            params![Kind::Agent.as_str(), project.id],
            |r| r.get(0),
        )?;
        let taken = |name: &str| -> Result<bool> {
            Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM accounts WHERE name = ?1)", [name], |r| r.get(0))?)
        };
        let mut n = count + 1;
        while taken(&format!("{} Agent {n}", project.name))? {
            n += 1;
        }
        Self::insert_account(c, &format!("{} Agent {n}", project.name), Kind::Agent, Some(project.id))
    }

    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let task = self.get_task(id)?;
        self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(task)
    }

    pub fn delete_tasks(&self, ids: &[i64]) -> Result<()> {
        self.write(|c| {
            for id in ids {
                c.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
            }
            Ok(())
        })
    }

    /// What a parent field asks for: "" is none, otherwise a task key like api#12 or a bare id.
    pub fn parent_of(&self, key: &str) -> Result<Option<i64>> {
        match key.trim() {
            "" => Ok(None),
            key => Ok(Some(self.find_task(key)?.id)),
        }
    }

    /// A task's subtasks, in the order they were made.
    pub fn subtasks(&self, id: i64) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare_cached(&format!("{} WHERE t.parent = ?1 ORDER BY t.id", Self::TASK_SELECT))?;
        let rows = stmt.query_map([id], Self::task_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A task can't be its own parent, or be under one of its own subtasks.
    fn check_parent(c: &Connection, task: i64, parent: i64) -> Result<()> {
        let mut at = Some(parent);
        while let Some(id) = at {
            if id == task {
                return Err(Error::Invalid("a task can't be under itself or one of its own subtasks".into()));
            }
            at = c
                .query_row("SELECT parent FROM tasks WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no task #{id}")))?;
        }
        Ok(())
    }

    pub fn list_comments(&self, task_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(&format!("{} WHERE task_id = ?1 ORDER BY id", Self::COMMENT_SELECT))?;
        let rows = stmt.query_map([task_id], Self::comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    const COMMENT_SELECT: &str =
        "SELECT id, task_id, author, body, created_at, moved_from, moved_to, assigned_from, assigned_to FROM comments";

    fn comment_row(r: &Row) -> rusqlite::Result<Comment> {
        Ok(Comment {
            id: r.get(0)?,
            task_id: r.get(1)?,
            author: r.get(2)?,
            body: r.get(3)?,
            created_at: r.get(4)?,
            moved_from: r.get(5)?,
            moved_to: r.get(6)?,
            assigned_from: r.get(7)?,
            assigned_to: r.get(8)?,
        })
    }

    pub fn add_comment(&self, task_id: i64, author: &str, body: &str, images: &[String]) -> Result<Comment> {
        let body = body.trim();
        if body.is_empty() && images.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        images.iter().try_for_each(|f| check_image(f))?;
        let author = match author.trim() {
            "" => BOARD_USER,
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
            .query_row(&format!("{} WHERE id = ?1", Self::COMMENT_SELECT), [id], Self::comment_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no comment {id}")))
    }

    pub fn delete_comment(&self, id: i64) -> Result<Comment> {
        let comment = self.get_comment(id)?;
        if comment.is_note() {
            return Err(Error::Invalid("a task's history can't be deleted".into()));
        }
        self.conn.execute("DELETE FROM comments WHERE id = ?1", [id])?;
        Ok(comment)
    }

    pub fn add_image(&self, task_id: i64, file: &str) -> Result<Image> {
        check_image(file)?;
        self.get_task(task_id)?;
        self.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, ?2)", params![task_id, file])?;
        Ok(Image { id: self.conn.last_insert_rowid(), task_id, comment_id: None, file: file.into() })
    }

    pub fn list_images(&self, task_id: i64) -> Result<Vec<Image>> {
        let mut stmt = self.conn.prepare_cached(&format!("{} WHERE task_id = ?1 ORDER BY id", Self::IMAGE_SELECT))?;
        let rows = stmt.query_map([task_id], Self::image_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    const IMAGE_SELECT: &str = "SELECT id, task_id, comment_id, file FROM images";

    fn image_row(r: &Row) -> rusqlite::Result<Image> {
        Ok(Image { id: r.get(0)?, task_id: r.get(1)?, comment_id: r.get(2)?, file: r.get(3)? })
    }

    pub fn delete_image(&self, id: i64) -> Result<Image> {
        let image = self
            .conn
            .query_row(&format!("{} WHERE id = ?1", Self::IMAGE_SELECT), [id], Self::image_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no image {id}")))?;
        self.conn.execute("DELETE FROM images WHERE id = ?1", [id])?;
        Ok(image)
    }

    pub fn image_files(&self, of: ImagesOf) -> Result<Vec<String>> {
        let (sql, id) = match of {
            ImagesOf::Tasks(ids) => {
                let mut files = Vec::new();
                for id in ids {
                    files.extend(self.image_files(ImagesOf::Task(id))?);
                }
                return Ok(files);
            }
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

pub enum ImagesOf {
    Image(i64),
    Comment(i64),
    Task(i64),
    Tasks(Vec<i64>),
    Project(i64),
}

pub const IMAGE_TYPES: [(&str, &str); 4] =
    [("png", "image/png"), ("jpg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp")];

/// The board takes images up to this size.
pub const IMAGE_LIMIT: usize = 25 * 1024 * 1024;

/// The extension of a PNG, JPEG, GIF or WebP image, from its first bytes, whatever its file is called.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        _ => None,
    }
}

pub fn tasks(n: i64) -> &'static str {
    if n == 1 { "task" } else { "tasks" }
}

pub fn valid_image_file(file: &str) -> bool {
    match file.split_once('.') {
        Some((hash, ext)) => {
            hash.len() == 16
                && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && IMAGE_TYPES.iter().any(|(e, _)| *e == ext)
        }
        None => false,
    }
}

fn check_image(file: &str) -> Result<()> {
    if valid_image_file(file) { Ok(()) } else { Err(Error::Invalid(format!("{file:?} is not an image Kanzo saved"))) }
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn short_time(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => ts.to_string(),
    }
}

/// Comments people wrote, not notes of moves or assignments.
const SAID: &str = "moved_to IS NULL AND assigned_to IS NULL";
const COMMENT_COUNT: &str =
    "(SELECT count(*) FROM comments WHERE task_id = t.id AND moved_to IS NULL AND assigned_to IS NULL)";
const IMAGE_COUNT: &str = "(SELECT count(*) FROM images WHERE task_id = t.id)";
/// Computed, never stored: reopening a task blocks what waits on it again, with nothing to keep in step.
const WAITING: &str = "EXISTS (SELECT 1 FROM task_waits w JOIN tasks b ON b.id = w.waits_on_id
    WHERE w.task_id = t.id AND b.status <> 'done')";
const QUALIFIERS: [&str; 15] = [
    "tag",
    "status",
    "project",
    "title",
    "description",
    "url",
    "comment",
    "id",
    "comments",
    "images",
    "created",
    "updated",
    "is",
    "has",
    "no",
];

/// A query as SQL to add to a WHERE clause: every part must match. A part is a word,
/// `"some words"`, or a qualifier from the README's table, which `-` negates. Anything that
/// isn't a complete qualifier, like `http://x` or `created:soon`, is a word.
#[derive(Default)]
struct Filter {
    sql: String,
    values: Vec<String>,
    first: usize,
}

impl Filter {
    fn new(query: &str, first: usize, hidden: &[String]) -> Self {
        let mut filter = Self { first, ..Default::default() };
        for (part, quoted) in query_parts(query).into_iter().take(10) {
            let part = part.to_lowercase();
            let qualifier = if quoted { None } else { filter.qualifier(&part, hidden) };
            let clause = qualifier.unwrap_or_else(|| filter.word(&part, hidden));
            if !clause.is_empty() {
                filter.sql.push_str(&format!(" AND {clause}"));
            }
        }
        filter
    }

    fn bind(&mut self, value: &str) -> String {
        self.values.push(value.to_string());
        format!("?{}", self.first + self.values.len() - 1)
    }

    fn word(&mut self, word: &str, hidden: &[String]) -> String {
        let seen = |property: &str| !hidden.iter().any(|h| h == property);
        let n = self.bind(word);
        let mut text = String::from("p.slug || '#' || t.id || ' ' || t.title");
        if seen("description") {
            text.push_str(" || ' ' || t.description");
        }
        if seen("tags") {
            text.push_str(" || ' ' || t.tags");
        }
        let mut sql = format!("has_word({text}, {n})");
        if seen("comments") {
            sql.push_str(&format!(" OR t.id IN (SELECT task_id FROM comments WHERE {SAID} AND has_word(body, {n}))"));
        }
        format!("({sql})")
    }

    /// `None` when `part` is a plain word. Empty while its value is still being typed.
    fn qualifier(&mut self, part: &str, hidden: &[String]) -> Option<String> {
        let (negated, part) = part.strip_prefix('-').map_or((false, part), |rest| (true, rest));
        let (name, value) = part.split_once(':')?;
        let property = match name {
            "tag" => "tags",
            "comment" => "comments",
            "is" | "has" | "no" => value,
            other => other,
        };
        if hidden.iter().any(|h| h == property) {
            return None;
        }
        if value.is_empty() && QUALIFIERS.contains(&name) {
            return Some(String::new());
        }
        let sql = match (name, value) {
            ("tag", tag) => format!("has_word(',' || t.tags || ',', {})", self.bind(&format!(",{tag},"))),
            ("status", status) => format!("t.status = {}", self.bind(status)),
            ("project", slug) => format!("p.slug = {}", self.bind(slug)),
            ("title", word) => format!("has_word(t.title, {})", self.bind(word)),
            ("description", word) => format!("has_word(t.description, {})", self.bind(word)),
            ("url", word) => format!("has_word(t.url, {})", self.bind(word)),
            ("comment", word) => {
                format!("t.id IN (SELECT task_id FROM comments WHERE {SAID} AND has_word(body, {}))", self.bind(word))
            }
            ("id", range) => range_sql("t.id", range, number)?,
            ("comments", range) => range_sql(COMMENT_COUNT, range, number)?,
            ("images", range) => range_sql(IMAGE_COUNT, range, number)?,
            ("created", range) => range_sql("substr(t.created_at, 1, 10)", range, date)?,
            ("updated", range) => range_sql("substr(t.updated_at, 1, 10)", range, date)?,
            ("is", "human") => "t.human".to_string(),
            ("is", "waiting") => WAITING.to_string(),
            ("has" | "no", "description") => "t.description <> ''".to_string(),
            ("has" | "no", "tags") => "t.tags <> ''".to_string(),
            ("has" | "no", "url") => "t.url <> ''".to_string(),
            ("has" | "no", "comments") => format!("{COMMENT_COUNT} > 0"),
            ("has" | "no", "images") => format!("{IMAGE_COUNT} > 0"),
            _ => return None,
        };
        Some(if negated != (name == "no") { format!("NOT ({sql})") } else { sql })
    }
}

/// Splits on spaces outside double quotes, and drops the quotes: `tag:"needs review"` is one
/// part. True when the part starts with a quote, which makes it a word whatever it says.
fn query_parts(query: &str) -> Vec<(String, bool)> {
    let mut parts = Vec::new();
    let (mut part, mut quoted, mut starts_quoted) = (String::new(), false, false);
    for c in query.chars() {
        match c {
            '"' => {
                starts_quoted |= part.is_empty() && !quoted;
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !part.is_empty() {
                    parts.push((std::mem::take(&mut part), starts_quoted));
                }
                starts_quoted = false;
            }
            c => part.push(c),
        }
    }
    if !part.is_empty() {
        parts.push((part, starts_quoted));
    }
    parts
}

/// `5`, `>5`, `>=5`, `<5`, `<=5` or `5..9`. `side` only lets through what's safe in SQL.
fn range_sql(column: &str, range: &str, side: fn(&str) -> Option<String>) -> Option<String> {
    if let Some((from, to)) = range.split_once("..") {
        return Some(format!("{column} BETWEEN {} AND {}", side(from)?, side(to)?));
    }
    for op in [">=", "<=", ">", "<"] {
        if let Some(rest) = range.strip_prefix(op) {
            return Some(format!("{column} {op} {}", side(rest)?));
        }
    }
    Some(format!("{column} = {}", side(range)?))
}

fn number(text: &str) -> Option<String> {
    text.parse::<i64>().ok().map(|n| n.to_string())
}

fn date(text: &str) -> Option<String> {
    let dashes = |i: usize, b: u8| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() };
    (text.len() == 10 && text.bytes().enumerate().all(|(i, b)| dashes(i, b))).then(|| format!("'{text}'"))
}

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

/// Any site will do, but only http and https: the board makes it a link people click.
fn clean_url(input: &str) -> Result<String> {
    let url = input.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    let host = url
        .split_once("://")
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next());
    if host.is_none_or(str::is_empty) || url.chars().any(char::is_whitespace) {
        return Err(Error::Invalid(format!("URL {url:?} should start with http:// or https://")));
    }
    Ok(url.to_string())
}

fn clean_repo(repo: &str) -> Result<String> {
    repo_from_url(repo).ok_or_else(|| Error::Invalid(format!("repo {:?} should look like owner/repo", repo.trim())))
}

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

pub fn repo_from_url(input: &str) -> Option<String> {
    let s = input.trim();
    let (host, path) = if let Some((_, rest)) = s.split_once("://") {
        rest.split_once('/')?
    } else {
        s.split_once(':').unwrap_or(("", s))
    };
    // A repo is its whole path, as GitLab nests them. A pasted page goes past it: on GitLab
    // after /-/, and on GitHub after owner/repo.
    let mut parts: Vec<&str> = path.split("/-/").next()?.split('/').filter(|p| !p.is_empty()).collect();
    if host.to_ascii_lowercase().ends_with("github.com") {
        parts.truncate(2);
    }
    let last = parts.last_mut()?;
    *last = last.strip_suffix(".git").unwrap_or(*last);
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (parts.len() >= 2 && parts.iter().all(|p| ok(p))).then(|| parts.join("/").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
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

    fn waits(keys: &str) -> TaskPatch {
        TaskPatch { waits_on: Some(keys.into()), ..Default::default() }
    }

    #[test]
    fn next_task_skips_a_task_until_what_it_waits_on_is_done() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocked = s.create_task_in(p.id, "blocked", TaskPatch { tags: Some("bug".into()), ..Default::default() });
        let blocked = blocked.unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let other = s.create_task(p.id, "other", "").unwrap();
        s.move_task(blocker.id, "review", None).unwrap();
        s.update_task(blocked.id, waits(&format!("#{}, api#{}", blocker.id, other.id))).unwrap();
        assert_eq!(s.get_task(blocked.id).unwrap().waiting_on, [blocker.id, other.id]);

        assert_eq!(s.next_task_matching(None, None, "tag:bug").unwrap().map(|t| t.id), None);
        assert_eq!(s.waiting_in_todo(None, None, "tag:bug").unwrap(), 1);
        assert_eq!(s.waiting_in_todo(None, None, "tag:nope").unwrap(), 0);
        assert_eq!(s.next_task(None, None).unwrap().map(|t| t.id), Some(other.id));
        s.move_task(other.id, "done", None).unwrap();
        assert_eq!(s.get_task(blocked.id).unwrap().waiting_on, [blocker.id]);
        assert_eq!(s.next_task(None, None).unwrap().map(|t| t.id), None, "review isn't done");

        s.move_task(blocker.id, "done", None).unwrap();
        assert!(s.get_task(blocked.id).unwrap().waiting_on.is_empty());
        s.move_task(blocker.id, "doing", None).unwrap();
        assert_eq!(s.next_task(None, None).unwrap().map(|t| t.id), None, "reopening blocks it again");
        s.move_task(blocker.id, "done", None).unwrap();
        assert_eq!(s.next_task(None, None).unwrap().map(|t| t.id), Some(blocked.id));
    }

    #[test]
    fn a_loop_or_a_bad_key_changes_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let [a, b, c] = ["a", "b", "c"].map(|t| s.create_task(p.id, t, "").unwrap().id);
        s.update_task(a, waits(&b.to_string())).unwrap();
        s.update_task(b, waits(&c.to_string())).unwrap();
        for keys in [a.to_string(), format!("{c}"), "999".to_string(), format!("nope#{a}")] {
            assert!(s.update_task(c, waits(&keys)).is_err(), "{keys}");
            assert!(s.get_task(c).unwrap().waiting_on.is_empty(), "{keys}");
        }
        assert!(s.update_task(a, waits(&format!("{c}, {a}"))).is_err());
        assert_eq!(s.get_task(a).unwrap().waiting_on, [b], "a refused list keeps the old one");

        let new = TaskPatch { waits_on: Some("999".into()), ..Default::default() };
        assert!(s.create_task_in(p.id, "bad", new).is_err());
        assert_eq!(s.list_tasks(None, None).unwrap().len(), 3, "a bad key creates no task");

        s.update_task(a, waits("")).unwrap();
        assert!(s.get_task(a).unwrap().waiting_on.is_empty());
    }

    #[test]
    fn deleting_what_a_task_waits_on_frees_it() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        s.move_task(blocker.id, "doing", None).unwrap();
        let new = TaskPatch { waits_on: Some(blocker.id.to_string()), ..Default::default() };
        let task = s.create_task_in(p.id, "task", new).unwrap();
        assert_eq!(task.waiting_on, [blocker.id]);
        assert_eq!(s.task_links(blocker.id, true).unwrap()[0].id, task.id);
        assert_eq!(s.task_links(task.id, false).unwrap()[0].id, blocker.id);
        s.delete_task(blocker.id).unwrap();
        assert_eq!(s.next_task(None, None).unwrap().map(|t| t.id), Some(task.id));
    }

    #[test]
    fn editing_the_same_list_changes_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let task = s.create_task(p.id, "task", "").unwrap();
        s.update_task(task.id, waits(&blocker.id.to_string())).unwrap();
        let before = s.changes().unwrap();
        s.update_task(task.id, waits(&format!("#{}", blocker.id))).unwrap();
        assert_eq!(s.changes().unwrap(), before);
    }

    #[test]
    fn filters_by_waiting_and_leaves_it_out_of_json_when_empty() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let task = s.create_task(p.id, "task", "").unwrap();
        s.update_task(task.id, waits(&blocker.id.to_string())).unwrap();
        assert_eq!(titles(&s.search("is:waiting", None).unwrap()), ["task"]);
        assert_eq!(s.search("is:waiting", None).unwrap()[0].waiting_on, [blocker.id]);
        assert_eq!(s.list_tasks(None, None).unwrap()[1].waiting_on, [blocker.id]);
        assert!(s.list_tasks(None, None).unwrap()[0].waiting_on.is_empty());
        assert_eq!(titles(&s.search("-is:waiting", None).unwrap()), ["blocker"]);
        let json = serde_json::to_value(s.get_task(blocker.id).unwrap()).unwrap();
        assert!(json.get("waiting_on").is_none(), "{json}");
        let json = serde_json::to_value(s.get_task(task.id).unwrap()).unwrap();
        assert_eq!(json["waiting_on"], serde_json::json!([blocker.id]));

        s.set_property("waiting", false).unwrap();
        assert_eq!(s.search_for_agent("is:waiting").unwrap().len(), 0, "a hidden property finds nothing");
    }

    #[test]
    fn changes_many_tasks_at_once_or_not_at_all() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let task = |title, tags: &str| {
            let tags = Some(tags.to_string());
            s.create_task_in(p.id, title, TaskPatch { tags, ..Default::default() }).unwrap().id
        };
        let (a, b, c) = (task("a", "bug, ui"), task("b", "ui"), task("c", "keep"));

        s.update_tasks(&[a, b], Some("doing"), "New, bug", "ui").unwrap();
        let tags = |id| s.get_task(id).unwrap().tags;
        assert_eq!(
            (tags(a), tags(b), tags(c)),
            (vec!["bug".into(), "new".into()], vec!["new".into(), "bug".into()], vec!["keep".into()])
        );
        assert_eq!(titles(&s.list_tasks(None, Some("doing")).unwrap()), ["a", "b"], "they keep their order");

        let before = s.changes().unwrap();
        assert!(s.update_tasks(&[c, 999], Some("done"), "x", "").is_err());
        assert!(s.update_tasks(&[c], Some("nope"), "", "").is_err());
        assert_eq!(
            (s.get_task(c).unwrap().status.as_str(), tags(c), s.changes().unwrap()),
            ("todo", vec!["keep".into()], before)
        );

        let before = s.get_task(a).unwrap();
        s.update_tasks(&[a], Some("doing"), "bug", "absent").unwrap();
        assert_eq!(s.get_task(a).unwrap().position, before.position, "nothing to change moves nothing");

        s.delete_tasks(&[a, 999, b]).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["c"]);
    }

    #[test]
    fn projects_keep_the_order_they_are_moved_to() {
        let (_dir, s) = store();
        for slug in ["a", "b", "c", "d"] {
            s.create_project(slug, "", None).unwrap();
        }
        let order = || s.list_projects().unwrap().into_iter().map(|p| p.slug).collect::<Vec<_>>().join("");
        let move_to = |slug, before| s.move_project(slug, before).unwrap();
        assert_eq!(order(), "abcd", "never moved: by slug");

        move_to("c", Some("a"));
        assert_eq!(order(), "cabd", "first");
        let positioned: i64 = s.conn.query_row("SELECT count(position) FROM projects", [], |r| r.get(0)).unwrap();
        assert_eq!(positioned, 1, "only the moved project needed a position");

        move_to("d", Some("b"));
        assert_eq!(order(), "cadb", "among projects with no position");
        move_to("c", None);
        assert_eq!(order(), "adbc", "last");
        move_to("b", Some("d"));
        assert_eq!(order(), "abdc", "between two");
        move_to("b", Some("b"));
        assert_eq!(order(), "abdc", "onto itself");

        s.create_project("0new", "", None).unwrap();
        assert_eq!(order(), "abdc0new", "a new project goes last");
        move_to("0new", Some("b"));
        assert_eq!(order(), "a0newbdc");

        for _ in 0..80 {
            // Out of float room between two neighbours: numbers them again.
            move_to("c", Some("b"));
            move_to("d", Some("b"));
        }
        assert_eq!(order(), "a0newcdb");
        assert!(s.move_project("a", Some("nope")).is_err());
        assert!(s.move_project("nope", None).is_err());
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
        let (tasks, hidden) = s.list_tasks_keeping_done(None, None, None, Some(2)).unwrap();
        assert_eq!((titles(&tasks), hidden), (vec!["open", "d3", "d4"], 2));
        let (tasks, hidden) = s.list_tasks_keeping_done(Some(p.id), Some("done"), None, Some(0)).unwrap();
        assert_eq!((tasks.len(), hidden), (0, 4));
        assert_eq!(s.list_tasks_keeping_done(None, Some("todo"), None, Some(0)).unwrap().1, 0);
        let (all, hidden) = s.list_tasks_keeping_done(None, None, None, None).unwrap();
        assert_eq!((titles(&all), hidden), (vec!["open", "d1", "d2", "d3", "d4"], 0));
    }

    #[test]
    fn opens_a_board_from_a_newer_kanzo_when_it_can() {
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
        set(newer, MIGRATIONS.len());
        s.migrate().unwrap();
        s.create_project("api", "", None).unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, newer.to_string());
        set(newer, newer);
        assert!(s.migrate().unwrap_err().to_string().contains("kanzo update"));
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
        assert_eq!((web.name.as_str(), web.color.as_deref()), ("web", None));
        assert_eq!(s.set_project_color("web", "teal").unwrap().color.as_deref(), Some("teal"));
        assert!(s.set_project_color("web", "#ff0000").is_err());
        let renamed = s.rename_project("web", "  Web   site ").unwrap();
        assert_eq!((renamed.slug.as_str(), renamed.name.as_str()), ("web", "Web site"));
        assert!(s.rename_project("web", "  ").is_err());
        assert!(s.find_project("nope").unwrap_err().to_string().contains("api, web"));
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
        let titles = |q: &str| s.search(q, Some(10)).unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles("LOGIN"), ["Fix login", "Login page copy"], "any project, open first, done too");
        assert_eq!(titles("login api"), ["Fix login"], "every word must match, the project too");
        assert_eq!(titles(&format!("web#{}", other.id)), ["Pricing 100%"], "by key");
        assert_eq!(titles("ui"), ["Pricing 100%"], "by tag");
        s.add_comment(old.id, "you", "Needs the new logo", &[]).unwrap();
        assert_eq!(titles("logo login"), ["Login page copy"], "by comment, with the task's own words");
        assert_eq!(titles("100%"), ["Pricing 100%"], "% is a plain character");
        assert!(titles("nothing-like-this").is_empty());
        assert_eq!(s.search("", Some(10)).unwrap().len(), 3, "no words: the latest changes");
        assert_eq!(s.search("login", Some(1)).unwrap()[0].id, login.id);
    }

    #[test]
    fn filter_by_any_property() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let patch = |tags: &str, status: &str| TaskPatch {
            tags: Some(tags.into()),
            status: Some(status.into()),
            description: Some("Users loop on /login".into()),
            ..Default::default()
        };
        let login = s.create_task_in(api.id, "Fix login", patch("Bug, needs review", "doing")).unwrap();
        let debug = s.create_task_in(api.id, "Debug tools", patch("debugger", "todo")).unwrap();
        let human = s.create_task(web.id, "Call the Bank", "").unwrap();
        s.update_task(
            human.id,
            TaskPatch { human: Some(true), url: Some("https://x.dev/1".into()), ..Default::default() },
        )
        .unwrap();
        s.add_comment(debug.id, "you", "Needs the new logo", &[]).unwrap();
        s.add_comment(debug.id, "you", "Soon", &[]).unwrap();
        s.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, 'a.png')", [human.id]).unwrap();
        s.conn
            .execute("UPDATE tasks SET created_at = '2026-01-15T10:00:00Z', updated_at = '2026-03-01T10:00:00Z'", [])
            .unwrap();
        s.conn.execute("UPDATE tasks SET created_at = '2026-02-01T10:00:00Z' WHERE id = ?1", [login.id]).unwrap();

        let ids = |q: &str| {
            let mut ids: Vec<i64> = s.search(q, None).unwrap().into_iter().map(|t| t.id).collect();
            ids.sort_unstable();
            ids
        };
        let (l, d, h) = (login.id, debug.id, human.id);
        for (q, want) in [
            ("tag:bug", vec![l]),
            ("TAG:BUG", vec![l]),
            ("tag:debug", vec![]),
            ("tag:\"needs review\"", vec![l]),
            ("-tag:bug", vec![d, h]),
            ("status:doing", vec![l]),
            ("status:DOING", vec![l]),
            ("project:api", vec![l, d]),
            ("-project:api", vec![h]),
            ("title:login", vec![l]),
            ("title:loop", vec![]),
            ("description:loop", vec![l, d]),
            ("url:x.dev", vec![h]),
            ("comment:logo", vec![d]),
            (&format!("id:{l}"), vec![l]),
            (&format!("id:>{l}"), vec![d, h]),
            (&format!("id:{l}..{d}"), vec![l, d]),
            ("comments:2", vec![d]),
            ("comments:>=1", vec![d]),
            ("comments:<1", vec![l, h]),
            ("images:1", vec![h]),
            ("created:2026-02-01", vec![l]),
            ("created:<2026-02-01", vec![d, h]),
            ("created:>=2026-02-01", vec![l]),
            ("created:2026-01-01..2026-01-31", vec![d, h]),
            ("updated:2026-03-01", vec![l, d, h]),
            ("is:human", vec![h]),
            ("-is:human", vec![l, d]),
            ("has:url", vec![h]),
            ("no:url", vec![l, d]),
            ("-has:url", vec![l, d]),
            ("-no:url", vec![h]),
            ("has:tags", vec![l, d]),
            ("has:description", vec![l, d]),
            ("has:comments", vec![d]),
            ("no:images", vec![l, d]),
            ("tag:bug status:doing", vec![l]),
            ("tag:bug status:todo", vec![]),
            ("project:api fix", vec![l]),
            ("project:api -status:doing", vec![d]),
            // Being typed: filters nothing yet.
            ("tag:", vec![l, d, h]),
            ("-status:", vec![l, d, h]),
            ("has:", vec![l, d, h]),
            // Not a complete qualifier: a word.
            ("created:soon", vec![]),
            ("comments:many", vec![]),
            ("is:nothing", vec![]),
            ("has:nothing", vec![]),
            ("http://x.dev", vec![]),
            ("-login", vec![]),
            // Quotes make a phrase, or text that only looks like a qualifier.
            ("\"loop on\"", vec![l, d]),
            ("\"tag:bug\"", vec![]),
        ] {
            assert_eq!(ids(q), want, "{q}");
        }
        s.update_task(l, TaskPatch { description: Some("status:doing is the text".into()), ..Default::default() })
            .unwrap();
        assert_eq!(ids("\"status:doing\""), [l]);
    }

    #[test]
    fn filter_values_cannot_change_the_sql() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        s.create_task(api.id, "Keep me", "").unwrap();
        for q in [
            "tag:x'OR'1'='1",
            "created:2026-01-01'OR'1'='1",
            "created:'OR'1'='1'--",
            "id:1)OR(1=1",
            "comments:1;DROP",
            "created:2026-01-0'",
            "title:'",
            "status:'--",
        ] {
            assert!(s.search(q, None).unwrap().is_empty(), "{q}");
        }
        assert_eq!(s.search("", None).unwrap().len(), 1);
    }

    #[test]
    fn next_task_takes_the_top_match_and_never_a_human_one() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let tagged = |title: &str, human: bool| {
            let patch = TaskPatch { tags: Some("billing".into()), human: Some(human), ..Default::default() };
            s.create_task_in(api.id, title, patch).unwrap();
        };
        s.create_task(api.id, "Not in the epic", "").unwrap();
        tagged("For the user", true);
        tagged("First in the epic", false);
        tagged("Second in the epic", false);

        let next = |q: &str| s.next_task_matching(Some(api.id), None, q).unwrap().map(|t| t.title);
        assert_eq!(next("is:human"), None);
        assert_eq!(next("tag:nope"), None);
        assert_eq!(next("tag:billing").as_deref(), Some("First in the epic"));
        assert_eq!(next("TAG:billing second").as_deref(), Some("Second in the epic"));
        assert_eq!(next("tag:billing"), None, "the human one stays");
        assert_eq!(next("  ").as_deref(), Some("Not in the epic"));
    }

    #[test]
    fn agents_cannot_filter_through_hidden_properties() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let patch = TaskPatch { tags: Some("secret".into()), ..Default::default() };
        let t = s.create_task_in(api.id, "Plain title", patch).unwrap();
        s.add_comment(t.id, "you", "whisper", &[]).unwrap();
        let found = |q: &str| s.search_for_agent(q).unwrap().len();
        assert_eq!((found("tag:secret"), found("secret"), found("comment:whisper"), found("whisper")), (1, 1, 1, 1));

        s.set_property("tags", false).unwrap();
        s.set_property("comments", false).unwrap();
        assert_eq!((found("tag:secret"), found("secret"), found("comment:whisper"), found("whisper")), (0, 0, 0, 0));
        assert_eq!(found("has:tags"), 0);
        assert_eq!(s.next_task_matching(None, None, "tag:secret").unwrap().map(|t| t.id), None);
        assert_eq!(found("plain"), 1, "what stays visible still matches");
        assert_eq!(s.search("tag:secret", None).unwrap().len(), 1, "the board still sees them");
    }

    #[test]
    fn search_ignores_case_beyond_ascii() {
        let (_dir, s) = store();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "Über café", "").unwrap();
        for q in ["über", "ÜBER", "Über", "café", "CAFÉ", "web#1"] {
            assert_eq!(s.search(q, None).unwrap().len(), 1, "{q}");
        }
    }

    #[test]
    fn images_are_known_by_their_bytes_not_their_names() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(image_type(b"\xff\xd8\xff\xe0...."), Some("jpg"));
        assert_eq!(image_type(b"GIF89a...."), Some("gif"));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WAVEfmt "), None, "a sound file");
        assert_eq!(image_type(b"<svg xmlns=..."), None, "SVG can carry script");
        assert_eq!(image_type(b""), None);
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
        let c = s.add_comment(t.id, "me", "", &["fedcba9876543210.jpg".into(), shot.clone()]).unwrap();
        assert!(s.add_comment(t.id, "me", "x", &["evil.svg".into()]).is_err());
        let all: Vec<_> = s.list_images(t.id).unwrap().into_iter().map(|i| (i.comment_id, i.file)).collect();
        assert_eq!(
            all,
            [(None, shot.clone()), (Some(c.id), "fedcba9876543210.jpg".into()), (Some(c.id), shot.clone())]
        );
        assert_eq!(s.image_files(ImagesOf::Task(t.id)).unwrap(), [shot.clone(), "fedcba9876543210.jpg".into()]);
        assert_eq!(s.image_files(ImagesOf::Project(p.id)).unwrap().len(), 2);
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
        let path = dir.path().join("kanzo.db");
        {
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
        let said = comments.iter().filter(|c| !c.is_note()).map(|c| c.author.as_str()).collect::<Vec<_>>();
        assert_eq!(said, ["claude-code", "me"]);
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
    fn next_task_takes_top_todo() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "web first", "").unwrap();
        let changes = s.changes().unwrap();
        let patch = TaskPatch { human: Some(true), tags: Some("Bug, bug".into()), ..Default::default() };
        let human = s.create_task_in(api.id, "Call the bank", patch).unwrap();
        assert!(human.human && human.tags == ["bug"] && s.changes().unwrap() == changes + 1, "all in one write");
        s.create_task(api.id, "api first", "").unwrap();
        s.create_task(api.id, "api second", "").unwrap();

        let t = s.next_task(Some(api.id), None).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("api first", "doing"));
        assert_eq!(s.next_task(None, None).unwrap().unwrap().title, "web first");
        assert_eq!(s.next_task(Some(api.id), None).unwrap().unwrap().title, "api second");
        assert!(s.next_task(Some(api.id), None).unwrap().is_none());
        assert_eq!(s.get_task(human.id).unwrap().status, "todo");
        assert!(!s.update_task(human.id, TaskPatch { human: Some(false), ..Default::default() }).unwrap().human);
    }

    #[test]
    fn account_names_are_unique_whatever_the_case() {
        let (_dir, s) = store();
        let me = s.create_account("  Ada   Lovelace ", Kind::Human, None).unwrap();
        assert_eq!((me.name.as_str(), me.kind), ("Ada Lovelace", Kind::Human));
        assert!(s.create_account("ada lovelace", Kind::Agent, None).is_err());
        assert!(s.create_account("  ", Kind::Agent, None).is_err());
        assert_eq!(s.account_by_token(&me.token).unwrap().name, "Ada Lovelace");
        assert_eq!(s.find_account("ADA LOVELACE").unwrap().token, me.token);
        assert!(s.account_by_token("kz_nope").is_err());
        let other = s.create_account("Bob", Kind::Human, None).unwrap();
        assert_ne!(other.token, me.token);
    }

    #[test]
    fn assign_agent_reuses_a_free_agent_or_names_a_new_one() {
        let (_dir, s) = store();
        let p = s.create_project("api", "Billing API", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        let c = s.create_task(p.id, "c", "").unwrap();

        let (first, created) = s.assign_agent(a.id).unwrap();
        assert_eq!((first.name.as_str(), first.kind, created), ("Billing API Agent 1", Kind::Agent, true));
        assert_eq!(first.project.as_deref(), Some("api"));
        // Agent 1 still has a to do, so b gets a new agent.
        let (second, created) = s.assign_agent(b.id).unwrap();
        assert_eq!((second.name.as_str(), created), ("Billing API Agent 2", true));
        // Once a's work is in review, agent 1 is free again.
        s.update_task(a.id, TaskPatch { status: Some(REVIEW.into()), ..Default::default() }).unwrap();
        let (again, created) = s.assign_agent(c.id).unwrap();
        assert_eq!((again.name.as_str(), created), ("Billing API Agent 1", false));
        // Reassigning a blocked task hands it to someone else: here nobody is free, so a new agent.
        s.update_task(b.id, TaskPatch { status: Some(BLOCKED.into()), ..Default::default() }).unwrap();
        let assign = |id| s.assign_agent(id).map(|(a, created)| (a.name, created)).unwrap();
        assert_eq!(assign(b.id), ("Billing API Agent 3".into(), true));
        // That left agent 2 with nothing to do.
        let d = s.create_task(p.id, "d", "").unwrap();
        assert_eq!(assign(d.id), ("Billing API Agent 2".into(), false));
        // A name someone took by hand is skipped.
        s.create_account("Billing API Agent 4", Kind::Human, None).unwrap();
        let e = s.create_task(p.id, "e", "").unwrap();
        assert_eq!(assign(e.id), ("Billing API Agent 5".into(), true));
    }

    #[test]
    fn assigned_tasks_belong_to_their_agent() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let free = s.create_task(p.id, "free", "").unwrap();
        let mine = s.create_task(p.id, "mine", "").unwrap();
        let agent = s.create_agent(&p).unwrap();
        let assign = |id, who| s.update_task(id, TaskPatch { assignee: Some(who), ..Default::default() }).unwrap();
        assert_eq!(assign(mine.id, Assignee::Account(agent.id)).assignee.as_deref(), Some("api Agent 1"));

        let listed = s.list_tasks_keeping_done(None, None, Some(agent.id), None).unwrap().0;
        assert_eq!(titles(&listed), ["mine"]);
        // Without an assignee, next_task leaves the agent's task alone.
        assert_eq!(s.next_task(None, None).unwrap().unwrap().id, free.id);
        assert!(s.next_task(None, None).unwrap().is_none());
        assert_eq!(s.next_task(None, Some(agent.id)).unwrap().unwrap().id, mine.id);

        assert!(assign(mine.id, Assignee::Nobody).assignee.is_none());
        assign(mine.id, Assignee::Account(agent.id));
        // Its agent gone, the task goes back to the AI agents' queue.
        s.delete_account("API AGENT 1").unwrap();
        assert_eq!(s.get_task(mine.id).unwrap().assignee.as_deref(), Some(AI_AGENTS));
    }

    #[test]
    fn status_changes_are_noted_among_the_comments() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "t", "").unwrap();
        let other = s.create_task(p.id, "other", "").unwrap();
        s.acting_as("Api Agent 1");
        s.next_task(None, None).unwrap();
        s.add_comment(t.id, "Api Agent 1", "halfway", &[]).unwrap();
        s.acting_as(BOARD_USER);
        s.update_task(t.id, TaskPatch { status: Some(REVIEW.into()), ..Default::default() }).unwrap();
        // Reordering within a column, or a patch that keeps the status, is no move.
        s.move_task(t.id, REVIEW, None).unwrap();
        s.update_task(t.id, TaskPatch { status: Some(REVIEW.into()), ..Default::default() }).unwrap();
        s.move_task(t.id, DONE, None).unwrap();

        let chain: Vec<String> = s
            .list_comments(t.id)
            .unwrap()
            .into_iter()
            .map(|c| match (c.moved_from, c.moved_to) {
                (Some(from), Some(to)) => format!("{from}>{to} {}", c.author),
                _ => c.body,
            })
            .collect();
        assert_eq!(chain, ["todo>doing Api Agent 1", "halfway", "doing>review me", "review>done me"]);
        assert_eq!(s.get_task(t.id).unwrap().comments, 1, "notes aren't comments");
        assert!(s.list_comments(other.id).unwrap().is_empty());
        let note = s.list_comments(t.id).unwrap().remove(0);
        assert!(s.delete_comment(note.id).is_err(), "the history stays");
        assert!(s.search("moved", None).unwrap().is_empty(), "notes aren't searched");
    }

    #[test]
    fn assignment_changes_are_noted_among_the_comments() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "t", "").unwrap();
        let ada = s.create_account("Ada", Kind::Human, None).unwrap();
        let assign = |who| s.update_task(t.id, TaskPatch { assignee: Some(who), ..Default::default() }).unwrap();
        assign(Assignee::Agents);
        assign(Assignee::Agents);
        s.acting_as("api Orchestrator");
        s.assign_agent(t.id).unwrap();
        s.acting_as("api Agent 1");
        assign(Assignee::Account(ada.id));
        assign(Assignee::Nobody);
        let notes: Vec<String> = s
            .list_comments(t.id)
            .unwrap()
            .into_iter()
            .inspect(|c| assert!(c.is_note()))
            .map(|c| format!("{} by {}", c.body, c.author))
            .collect();
        assert_eq!(
            notes,
            [
                "assigned to AI Agents by me",
                "reassigned from AI Agents to api Agent 1 by api Orchestrator",
                "reassigned from api Agent 1 to Ada by api Agent 1",
                "unassigned from Ada by api Agent 1",
            ],
            "the same assignee twice is no change"
        );
        assert_eq!(s.get_task(t.id).unwrap().comments, 0);
    }

    #[test]
    fn tasks_queued_for_ai_agents_wait_for_an_orchestrator() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let me = s.create_account("Ada", Kind::Human, None).unwrap();
        assert!(s.create_account("ai agents", Kind::Human, None).is_err(), "the queue's name is taken");
        let mine = s.create_task(p.id, "mine", "").unwrap();
        let queued = s.create_task(p.id, "queued", "").unwrap();
        let assign = |id, who| s.update_task(id, TaskPatch { assignee: Some(who), ..Default::default() }).unwrap();
        assign(mine.id, Assignee::Account(me.id));
        assert!(s.queued_for_agents(p.id).unwrap().is_none(), "nothing queued yet");
        assert_eq!(assign(queued.id, Assignee::Agents).assignee.as_deref(), Some(AI_AGENTS));
        assert_eq!(s.queued_for_agents(p.id).unwrap().unwrap().id, queued.id);

        let (agent, _) = s.assign_agent(queued.id).unwrap();
        assert_eq!(s.get_task(queued.id).unwrap().assignee, Some(agent.name));
        assert!(s.queued_for_agents(p.id).unwrap().is_none(), "handed to an agent");
        // A human's task isn't AI agents' work, and no agent takes it.
        assert!(s.next_task(None, None).unwrap().is_none());
        assign(mine.id, Assignee::Nobody);
        assert_eq!(s.next_task(None, None).unwrap().unwrap().id, mine.id);
    }

    #[test]
    fn backlog_is_a_column_agents_skip() {
        let (dir, s) = store();
        assert_eq!(s.statuses_for(None).unwrap(), STATUSES);
        s.set_backlog(true).unwrap();
        let s = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
        assert_eq!(s.statuses_for(None).unwrap(), ["backlog", "todo", "doing", "blocked", "review", "done"]);
        let api = s.create_project("api", "", None).unwrap();
        let status = |s: &str| TaskPatch { status: Some(s.into()), ..Default::default() };
        let backlog = s.create_task_in(api.id, "later", status("backlog")).unwrap();
        assert_eq!(backlog.status, "backlog");
        assert!(s.create_task_in(api.id, "x", status("nope")).is_err());
        assert_eq!(s.create_task(api.id, "now", "").unwrap().status, "todo");

        let t = s.next_task(None, None).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("now", "doing"));
        assert!(s.next_task(None, None).unwrap().is_none());
        assert_eq!(s.get_task(backlog.id).unwrap().status, "backlog");

        assert!(s.set_backlog(false).is_err());
        s.delete_task(backlog.id).unwrap();
        s.set_backlog(false).unwrap();
    }

    #[test]
    fn hidden_properties_keep_their_data() {
        let (_dir, s) = store();
        assert!(s.hidden_properties().unwrap().is_empty());
        let api = s.create_project("api", "", None).unwrap();
        let t = s.create_task(api.id, "x", "Why").unwrap();
        let before = s.changes().unwrap();

        s.set_property("description", false).unwrap();
        s.set_property("tags", false).unwrap();
        s.set_property("tags", false).unwrap();
        assert_eq!(s.hidden_properties().unwrap(), ["description", "tags"]);
        assert!(s.changes().unwrap() > before, "other open boards redraw");
        assert_eq!(s.get_task(t.id).unwrap().description, "Why");

        s.set_property("description", true).unwrap();
        assert_eq!(s.hidden_properties().unwrap(), ["tags"]);
        for always in ["title", "status", "nope"] {
            assert!(s.set_property(always, false).is_err(), "{always}");
        }
    }

    #[test]
    fn tasks_break_into_subtasks() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let epic = s.create_task(p.id, "Epic", "").unwrap();
        let a = s.create_task(p.id, "Part A", "").unwrap();
        let b = s.create_task(p.id, "Part B", "").unwrap();
        let under = |id, parent| s.update_task(id, TaskPatch { parent: Some(parent), ..Default::default() });
        assert_eq!(under(a.id, Some(epic.id)).unwrap().parent.as_deref(), Some("api#1"));
        under(b.id, Some(epic.id)).unwrap();
        let grand = s.create_task(p.id, "Part A.1", "").unwrap();
        under(grand.id, Some(a.id)).unwrap();
        let titles = |id| s.subtasks(id).unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles(epic.id), ["Part A", "Part B"]);
        assert_eq!(titles(a.id), ["Part A.1"]);

        assert!(under(epic.id, Some(epic.id)).is_err(), "not under itself");
        assert!(under(epic.id, Some(grand.id)).is_err(), "not under its own subtask's subtask");
        assert!(under(a.id, Some(999)).is_err(), "no such task");
        assert_eq!(s.parent_of(" api#3 ").unwrap(), Some(b.id));
        assert_eq!(s.parent_of("").unwrap(), None);
        assert!(under(b.id, None).unwrap().parent.is_none(), "a task of its own again");

        // Deleting a parent leaves its subtasks as tasks of their own.
        s.delete_task(a.id).unwrap();
        assert!(s.get_task(grand.id).unwrap().parent.is_none());

        // A template's subtasks stay under the copy of their parent.
        under(grand.id, Some(epic.id)).unwrap();
        s.set_project_template("api", true).unwrap();
        let copy = s.create_project_from("web", "", None, "api").unwrap();
        let copied = s.list_tasks(Some(copy.id), None).unwrap();
        let epic2 = copied.iter().find(|t| t.title == "Epic").unwrap();
        assert_eq!(titles(epic2.id), ["Part A.1"]);
        assert_eq!(s.subtasks(epic2.id).unwrap()[0].project, "web", "under the copy, not the template");
    }

    #[test]
    fn projects_start_from_a_template() {
        let (_dir, s) = store();
        let tpl = s.create_project("release", "Release", None).unwrap();
        s.add_status(tpl.id, "qa", "A person checks it", None, Some("eye")).unwrap();
        let ada = s.create_account("Ada", Kind::Human, None).unwrap();
        let agent = s.create_agent(&tpl).unwrap();
        let assign = |id, who| s.update_task(id, TaskPatch { assignee: Some(who), ..Default::default() }).unwrap();
        let notes = s.create_task(tpl.id, "Write notes", "Changelog").unwrap();
        s.update_task(notes.id, TaskPatch { tags: Some("docs".into()), ..Default::default() }).unwrap();
        assign(notes.id, Assignee::Account(ada.id));
        let tag = s.create_task(tpl.id, "Tag the release", "").unwrap();
        assign(tag.id, Assignee::Account(agent.id));
        let check = s.create_task(tpl.id, "Check it", "").unwrap();
        s.move_task(check.id, "qa", None).unwrap();
        let old = s.create_task(tpl.id, "Last time's", "").unwrap();
        s.move_task(old.id, DONE, None).unwrap();
        s.add_comment(notes.id, "me", "only in the template", &[]).unwrap();

        assert!(s.create_project_from("v2", "", None, "release").is_err(), "not a template yet");
        assert!(s.find_project("v2").is_err(), "and nothing was left behind");
        assert!(s.set_project_template("release", true).unwrap().template);
        // The template's own tasks wait: agents never take them.
        assert!(s.next_task(None, None).unwrap().is_none());
        assert!(s.queued_for_agents(tpl.id).unwrap().is_none());

        let v2 = s.create_project_from("v2", "Version 2", None, "release").unwrap();
        assert!(!v2.template);
        let qa = &s.custom_statuses(Some(v2.id)).unwrap()[0];
        assert_eq!((qa.name.as_str(), qa.icon.as_deref()), ("qa", Some("eye")));
        let copied = s.list_tasks(Some(v2.id), None).unwrap();
        let summary: Vec<_> =
            copied.iter().map(|t| (t.title.as_str(), t.status.as_str(), t.assignee.as_deref())).collect();
        assert_eq!(
            summary,
            [
                ("Write notes", "todo", Some("Ada")),
                ("Tag the release", "todo", Some(AI_AGENTS)),
                ("Check it", "qa", None),
            ],
            "done tasks stay behind; an agent's task goes back to the AI Agents queue"
        );
        assert_eq!((copied[0].description.as_str(), copied[0].tags.clone()), ("Changelog", vec!["docs".to_string()]));
        assert_eq!(copied[0].comments, 0, "comments stay with the template");
        assert!(s.list_comments(copied[0].id).unwrap().is_empty(), "and no notes about the copy");
        // The copy's tasks are real work.
        assert_eq!(s.queued_for_agents(v2.id).unwrap().unwrap().title, "Tag the release");
        assert!(s.create_project_from("v3", "", None, "nope").is_err());
    }

    #[test]
    fn projects_add_their_own_columns() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let qa = s.add_status(api.id, " QA ", "A person tests it", None, Some("Eye")).unwrap();
        assert_eq!((qa.name.as_str(), qa.after.as_str(), qa.icon.as_deref()), ("qa", "review", Some("eye")));
        assert!(s.add_status(api.id, "x", "x", None, Some("nope")).is_err(), "unknown icon");
        assert_eq!(s.set_status_icon(api.id, "qa", Some("")).unwrap().icon, None);
        assert_eq!(s.set_status_icon(api.id, "QA", Some("bug")).unwrap().icon.as_deref(), Some("bug"));
        assert!(s.set_status_icon(api.id, "doing", Some("bug")).is_err(), "only a project's own columns");
        s.add_status(api.id, "staging", "Deployed to staging", None, None).unwrap();
        s.add_status(api.id, "spec", "Needs a spec", Some("todo"), None).unwrap();
        s.add_status(api.id, "spec review", "Spec under review", Some("spec"), None).unwrap();
        assert_eq!(
            s.statuses_for(Some(api.id)).unwrap(),
            ["todo", "spec", "spec review", "doing", "blocked", "review", "qa", "staging", "done"]
        );
        assert_eq!(s.statuses_for(Some(web.id)).unwrap(), STATUSES);
        assert!(s.add_status(api.id, "qa", "again", None, None).is_err(), "taken in this project");
        assert!(s.add_status(api.id, "doing", "x", None, None).is_err(), "a column every project has");
        assert!(s.add_status(api.id, "late", "x", Some("done"), None).is_err(), "done stays last");
        assert!(s.add_status(api.id, "late", "  ", None, None).is_err(), "agents need a description");
        s.add_status(web.id, "qa", "Web QA", None, None).unwrap();

        let t = s.create_task(api.id, "t", "").unwrap();
        let other = s.create_task(web.id, "o", "").unwrap();
        assert_eq!(s.move_task(t.id, "staging", None).unwrap().status, "staging");
        assert!(s.move_task(other.id, "staging", None).is_err(), "web has no staging");
        let to_web = TaskPatch { project_id: Some(web.id), ..Default::default() };
        assert!(s.update_task(t.id, to_web).is_err(), "its column would be gone");

        assert!(s.remove_status(api.id, "staging").is_err(), "a task is still there");
        s.move_task(t.id, "done", None).unwrap();
        s.remove_status(api.id, "staging").unwrap();
        // What came after spec moves up to its place.
        s.remove_status(api.id, "spec").unwrap();
        assert_eq!(
            s.statuses_for(Some(api.id)).unwrap(),
            ["todo", "spec review", "doing", "blocked", "review", "qa", "done"]
        );
        s.delete_project("api").unwrap();
        assert_eq!(s.custom_statuses(None).unwrap().len(), 1);
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
    fn a_task_links_to_one_url_on_any_site() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let t = s.create_task(api.id, "Fix login", "").unwrap();
        assert_eq!(t.url, "");
        let set = |url: &str| s.update_task(t.id, TaskPatch { url: Some(url.into()), ..Default::default() });
        for url in [
            "https://github.com/Me/Api/pull/12",
            "https://gitlab.example.com/a/b/-/merge_requests/3?x=1#y",
            "http://intranet",
        ] {
            assert_eq!(set(&format!("  {url} ")).unwrap().url, url);
        }
        for bad in [
            "#12",
            "owner/repo#12",
            "github.com/a/b",
            "javascript:alert(1)",
            "javascript://x",
            "https://",
            "https://a b",
        ] {
            assert!(set(bad).unwrap_err().to_string().contains("should start with"), "{bad}");
        }
        assert_eq!(s.get_task(t.id).unwrap().url, "http://intranet", "a bad URL changes nothing");
        assert_eq!(
            s.update_task(t.id, TaskPatch { title: Some("Fix".into()), ..Default::default() }).unwrap().url,
            "http://intranet"
        );
        assert_eq!(set("").unwrap().url, "");
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
            "https://gitea.example.com/owner/repo.git",
            "git@gitea.example.com:owner/repo.git",
            "ssh://git@gitea.example.com:2222/owner/repo.git",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("owner/repo"), "{url}");
        }
        for url in [
            "acme/backend/api",
            "https://gitlab.com/acme/backend/api",
            "https://gitlab.com/acme/backend/api.git",
            "git@gitlab.com:acme/backend/api.git",
            "ssh://git@gitlab.com/acme/backend/api.git",
            "https://gitlab.com/acme/backend/api/-/merge_requests/3",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("acme/backend/api"), "{url}");
        }
        assert_eq!(repo_from_url("repo"), None);
        assert_eq!(repo_from_url("own er/repo"), None);
    }

    #[test]
    fn repos_nested_in_one_group_are_different_repos() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("https://gitlab.com/acme/backend/api.git")).unwrap();
        let web = s.create_project("web", "", Some("git@gitlab.com:acme/backend/web.git")).unwrap();
        assert_eq!(api.repos, ["acme/backend/api"]);
        assert_eq!(s.project_by_repo("acme/backend/web").unwrap().unwrap().id, web.id);
        assert!(s.create_project("api2", "", Some("acme/backend/api")).is_err(), "still one project per repo");
    }

    #[test]
    fn moves_several_tasks_in_one_write() {
        let (_tmp, s) = store();
        let p = s.create_project("api", "", None).unwrap().id;
        let task = |title: &str| s.create_task(p, title, "").unwrap().id;
        let (a, b, c, d, x, y) = (task("a"), task("b"), task("c"), task("d"), task("x"), task("y"));
        s.move_tasks(&[x, y], "doing", None).unwrap();
        let order =
            |status: &str| -> Vec<i64> { s.list_tasks(None, Some(status)).unwrap().iter().map(|t| t.id).collect() };

        s.move_tasks(&[c, a], "doing", Some(y)).unwrap();
        assert_eq!(order("doing"), [x, c, a, y]);
        assert_eq!(order("todo"), [b, d]);

        // To the end, within a column, and past an anchor that's moving too.
        s.move_tasks(&[x, c], "doing", None).unwrap();
        assert_eq!(order("doing"), [a, y, x, c]);
        s.move_tasks(&[a, y], "doing", Some(a)).unwrap();
        assert_eq!(order("doing"), [x, c, a, y]);

        // More moves than there is float room between two neighbours.
        let many: Vec<i64> = (0..80).map(|i| task(&format!("m{i}"))).collect();
        s.move_tasks(&many, "doing", Some(y)).unwrap();
        let doing = order("doing");
        assert_eq!(&doing[3..83], &many[..]);
        assert_eq!(doing.last(), Some(&y));

        // All or nothing.
        assert!(s.move_tasks(&[b, 99999], "doing", None).is_err());
        assert!(order("todo").contains(&b));
        assert!(s.move_tasks(&[b], "nope", None).is_err());
    }
}
