# Kanzo

A local kanban board for you and your coding agents.

One small program serves the board in your browser and an MCP server for agents like Claude Code and Codex. Everything lives in one folder you can copy, or sync between laptops through a private GitHub repo.

- **Minimal:** projects, tasks, four columns, Markdown descriptions and comments.
- **One binary,** about 5 MB, with SQLite built in. No runtime, no Docker, no npm.
- **MCP first:** agents can do everything you can do on the board.
- **Multi-project:** one board for all your projects. Link a project to a GitHub repo and agents working in that repo find it on their own.

## Install

```sh
brew install emanuele-em/tap/kanzo
```

Or use the one-line installer from the [latest release](https://github.com/emanuele-em/kanzo/releases/latest), or build from source with `cargo install --path .`.

## Use it

```sh
kanzo
```

The board opens at http://localhost:4242. The first run creates `~/kanzo`.

Press `n` to add a task.

To keep the board running in the background and start it at login:

```sh
kanzo service install     # kanzo service uninstall to remove it
```

### Connect your agents

Do this once per machine.

```sh
claude mcp add kanzo --scope user -- kanzo mcp    # Claude Code
codex mcp add kanzo -- kanzo mcp                  # Codex
```

Any other MCP client works too: the command is `kanzo` with the argument `mcp`. GUI apps such as Raycast may not see Homebrew's `PATH`, so give them the full path from `which kanzo`.

Then ask your agent things like *"take the next task from kanzo and do it"*, or *"add a kanzo task for the flaky login test"*.

### Tools agents get

| Tool | Does |
| --- | --- |
| `list_projects` | Lists projects with open task counts |
| `create_project` | Adds a project, optionally linked to a GitHub repo |
| `list_tasks` | Lists tasks one per line; done tasks are left out unless asked for |
| `get_task` | Shows a task with its description and comments |
| `create_task` | Adds a task to `todo` |
| `update_task` | Changes title, description or status |
| `add_comment` | Adds a comment, signed with the agent's name |
| `next_task` | Takes the top `todo` task and moves it to `doing` |
| `delete_task` | Deletes a task and its comments |

Tasks are written `api#12`. When an agent leaves out the project, Kanzo reads the git remote of the folder the agent works in and picks the project linked to that repo. If no project matches, the tool says so and lists the projects to choose from.

## The data folder

Everything is in `~/kanzo`. Use another folder with `--home <dir>` or `KANZO_HOME`.

| File | Holds |
| --- | --- |
| `kanzo.db` | The board (SQLite) |
| `config.toml` | Port and columns |
| `.gitignore` | Keeps SQLite's temporary files and local files out of git |

`config.toml`:

```toml
port = 4242
statuses = ["todo", "doing", "review", "done"]
```

New tasks go in the first column; tasks in the last one count as done.

### Export

Read the board without Kanzo, or keep a plain-text copy:

```sh
kanzo export                       # Markdown, to the terminal
kanzo export -f json -o board.json # JSON, to a file
kanzo export -p api                # one project
```

## Sync between laptops

The data folder can be a private GitHub repo. `kanzo sync` commits the board and pulls or pushes it.

1. Once, on the first laptop:
   ```sh
   cd ~/kanzo && git init && gh repo create kanzo-data --private --source . --push
   ```
2. On the second laptop, the first time: `git clone` that repo into `~/kanzo`.
3. Run `kanzo sync` on the laptop you are leaving, and again on the one you sit down at.

Sync is safe while the board and agents are running: it waits for their writes to finish, and they pick up a pulled board on their next action.

A database file can't be merged. If both laptops changed the board since the last sync, `kanzo sync` stops and asks which one to keep:

```sh
kanzo sync --keep-local    # overwrite GitHub with this laptop's board
kanzo sync --keep-remote   # take GitHub's board; this laptop's is kept in kanzo.db.bak
```

**Without GitHub:** stop `kanzo`, copy `~/kanzo` to the other Mac, done.

## Development

```sh
cargo test               # store, config, export and service tests
cargo run                # the board, with data in ~/kanzo
cargo run -- --home /tmp/k --port 4300   # a throwaway board
```

The code is small on purpose:

| File | Does |
| --- | --- |
| `src/main.rs` | Command line |
| `src/home.rs` | Data folder and `config.toml` |
| `src/schema.sql` | The database schema |
| `src/store.rs` | All SQL |
| `src/mcp.rs` | The MCP tools (`rmcp`, pinned on purpose: it still changes often) |
| `src/web.rs` | The board's HTTP API |
| `src/index.html` | The whole board UI: plain HTML, CSS and JavaScript, built into the binary |
| `src/sync.rs` | `kanzo sync` |
| `src/export.rs` | `kanzo export` |
| `src/service.rs` | `kanzo service` |

Every operation opens its own short-lived SQLite connection, in WAL mode with a 5-second busy timeout, so the board and any number of agents can write at once. The board polls a change counter every 2 seconds, which database triggers bump on every write.

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist): push a tag like `v0.1.0` and GitHub Actions builds both Mac binaries, the installer and the Homebrew formula.
