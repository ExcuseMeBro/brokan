# Kanzo guide

Everything about Kanzo beyond the [README](../README.md): installing, running it in the background, agents, the data folder, sync, and how it stays light.

## Install

Use the one-line installer from the [latest release](https://github.com/emanuele-em/kanzo/releases/latest), or build from source with `cargo install --path .`.

Releases include:

| Build | Runs on |
| --- | --- |
| `aarch64-apple-darwin` | Apple Silicon Macs, macOS 11 or later |
| `x86_64-apple-darwin` | Intel Macs, macOS 10.12 Sierra or later |
| `x86_64-unknown-linux-musl` | 64-bit PCs with any Linux |
| `aarch64-unknown-linux-musl` | Raspberry Pi 3, 4 and 5 with a 64-bit OS, and other ARM64 boards |
| `armv7-unknown-linux-musleabihf` | Raspberry Pi 2 and 3 with a 32-bit OS |
| `arm-unknown-linux-musleabihf` | Raspberry Pi Zero and 1 (ARMv6) |

The Linux builds are static, so they work on any distribution, however old its C library.

## Use it

```sh
kanzo
```

The board opens at http://localhost:4242. The first run creates `~/kanzo`.

Press `n` to add a task and `/` to search. Search matches every word you type against task ids, titles, descriptions and project names.

To keep the board running in the background and start it at login:

```sh
kanzo service install     # kanzo service uninstall to remove it
```

This is a LaunchAgent on macOS and a systemd user service on Linux. On a board with no screen, run `sudo loginctl enable-linger $USER` once so it starts at boot without anyone logging in.

The board works in browsers back to Safari 13.1, Chrome 80 and Firefox 78.

### Use it from another device

On a Raspberry Pi with no screen, set `listen = "0.0.0.0"` in `config.toml`, then open `http://<pi-name>.local:4242` from your laptop. Kanzo has no logins, so only do this on a network you trust: anyone who can reach the port can read and change the board.

### Connect your agents

Do this once per machine.

```sh
claude mcp add kanzo --scope user -- kanzo mcp    # Claude Code
codex mcp add kanzo -- kanzo mcp                  # Codex
```

Any other MCP client works too: the command is `kanzo` with the argument `mcp`. GUI apps such as Raycast may not see your shell's `PATH`, so give them the full path from `which kanzo`.

**Over HTTP.** Clients that can't start a local program can use the running board instead, at `http://localhost:4242/mcp`:

```sh
claude mcp add --transport http kanzo http://localhost:4242/mcp
```

Over HTTP Kanzo can't see which folder the agent works in, so the agent has to name the project. The endpoint is reachable wherever the board is (see `listen`); turn it off with `mcp_http = false` in `config.toml`.

Then ask your agent things like *"take the next task from kanzo and do it"*, or *"add a kanzo task for the flaky login test"*.

### Tools agents get

| Tool | Does |
| --- | --- |
| `list_projects` | Lists projects with open task counts |
| `create_project` | Adds a project, optionally linked to a GitHub repo |
| `list_tasks` | Lists tasks one per line; done tasks are left out unless asked for, then the newest 20 |
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
| `config.toml` | Port, address, columns |
| `.gitignore` | Keeps SQLite's temporary files and local files out of git |

`config.toml`:

```toml
port = 4242
listen = "127.0.0.1"     # "0.0.0.0" to open the board from other devices
statuses = ["todo", "doing", "review", "done"]
mcp_http = true
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
cargo test               # store, config, export, web and service tests
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
| `src/mcp.rs` | The MCP tools, over stdio and HTTP (`rmcp`, pinned on purpose: it still changes often) |
| `src/web.rs` | The board's HTTP API, and `/mcp` |
| `src/index.html`, `src/app.js`, `src/logo.png` | The board UI: plain HTML, CSS and JavaScript, built into the binary |
| `src/sync.rs` | `kanzo sync` |
| `src/export.rs` | `kanzo export` |
| `src/service.rs` | `kanzo service` |

### How it stays light

- **One thread** runs the server; database work goes to at most 8 helper threads, which exit when idle.
- **One short SQLite connection per operation,** with a 5-second busy timeout, so the board and any number of agents can write at once, and `kanzo sync` can safely swap the file between operations. The database uses a rollback journal instead of WAL, because with short connections WAL created and deleted two files on every request. Now a read touches no extra files, and each operation is about 3 times faster.
- **Cheap polling:** the board asks for a change token every 2 seconds, only while its tab is visible, and waits for each answer before asking again. Database triggers bump the token on every write.
- **Small pages:** the board loads only the newest 50 done tasks until you ask for more or search, so a board that has piled up thousands of done tasks reloads as fast as a new one.
- **Few tokens for agents:** MCP replies are short plain text, and the tool list agents read at the start of every session is kept small (about 3 KB).
- **Release profile:** `opt-level = "s"` with full LTO measured 30% smaller than the default at the same speed.

Measured on a 64-bit ARM Linux container limited to 64 MB of memory and half a CPU: 3.5 MB of memory and one thread when idle, 4.3 MB after 200 tasks.

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist): push a tag like `v0.1.0` and GitHub Actions builds every target above and the installer. CI runs `cargo fmt`, `clippy` and the tests on Linux and macOS for every push.
