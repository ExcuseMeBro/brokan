-- Kanzo schema, version 1.
-- Later changes go in new numbered migrations in store.rs; never edit this one after release.

CREATE TABLE projects (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    slug       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    repo       TEXT UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL,
    position    REAL NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX tasks_by_column ON tasks(status, position);
CREATE INDEX tasks_by_project ON tasks(project_id);

CREATE TABLE comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    author     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX comments_by_task ON comments(task_id);

-- The change counter the board polls. Every write to any table bumps it, so
-- changes made by agents through `kanzo mcp` show up on the board.
INSERT INTO meta (key, value) VALUES ('changes', '0');

CREATE TRIGGER projects_insert AFTER INSERT ON projects BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER projects_update AFTER UPDATE ON projects BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER projects_delete AFTER DELETE ON projects BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER tasks_insert    AFTER INSERT ON tasks    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER tasks_update    AFTER UPDATE ON tasks    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER tasks_delete    AFTER DELETE ON tasks    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER comments_insert AFTER INSERT ON comments BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER comments_update AFTER UPDATE ON comments BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
CREATE TRIGGER comments_delete AFTER DELETE ON comments BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
