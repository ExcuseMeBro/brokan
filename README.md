<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/kanzo-dark.jpg">
    <img alt="kanzo: a tiny kanban board for you and your coding agents" src="docs/img/kanzo-light.jpg">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml"><img src="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="AGPL-3.0"></a>
</p>

Kanzo is a kanban board in your browser and an MCP server in one small program, so you and your coding agents work on the same tasks.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/board-dark.jpg">
  <img alt="The Kanzo board: a project with todo, doing, review and done columns" src="docs/img/board-light.jpg">
</picture>

## Install

```sh
curl -LsSf https://github.com/emanuele-em/kanzo/releases/latest/download/kanzo-installer.sh | sh
```

For macOS and Linux, including every Raspberry Pi. Or build it: `cargo install --git https://github.com/emanuele-em/kanzo`.

## Use

| Command | Does |
| --- | --- |
| `kanzo` | Starts the board at http://localhost:4242 and opens it; `--port` for another port, `--no-open` to not open it |
| `kanzo service install` | Keeps the board running and starts it at login; `uninstall` removes it |
| `kanzo mcp` | The MCP server for agents: they start it, not you |
| `kanzo sync` | Commits the board and pulls or pushes it; `--keep-local` or `--keep-remote` when both sides changed |
| `kanzo export` | Prints the board as Markdown; `-f json` for JSON, `-p <project>` for one project, `-o <file>` to save it |
| `kanzo init` | Creates the data folder; the first run does this too |
| `kanzo --version` | Prints the version |

Every command takes `--home <dir>` to use another data folder.

`n` adds a task, `/` searches, `Tab` moves to the description, `⌘↵` saves and closes, `Esc` cancels.

Paste an image into a task and Kanzo saves it in `~/kanzo/images` and types its path, like a terminal does, so your agents can open it.

When a new version is out, the sidebar offers it: one click installs it and restarts the board.

## Connect your agents

```sh
claude mcp add kanzo --scope user -- kanzo mcp   # Claude Code
codex mcp add kanzo -- kanzo mcp                 # Codex
```

Other MCP clients: run `kanzo mcp` (apps like Raycast need the full path, from `which kanzo`), or use the running board at `http://localhost:4242/mcp`.

Then ask: *"take the next task from kanzo and do it."* The agent moves the card to `doing` and comments as it works. It finds the right project from the git remote of the repo it works in; link more repos to a project from its ⋯ menu.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/task-dark.jpg">
  <img alt="A task open on the board, with a description and comments from Claude Code" src="docs/img/task-light.jpg">
</picture>

## Settings

Everything lives in `~/kanzo`: the board in `kanzo.db`, settings in `config.toml`. Use another folder with `--home` or `KANZO_HOME`.

```toml
port = 4242
listen = "127.0.0.1"   # "0.0.0.0" opens the board to your network: no logins, so only on one you trust
statuses = ["todo", "doing", "review", "done"]   # new tasks go first, the last counts as done
mcp_http = true        # MCP at /mcp
```

## Sync between machines

Make `~/kanzo` a private git repo once (`git init`, then push it to GitHub), clone it on your other machines, and run `kanzo sync` when you switch. It is safe while the board and agents run.

## Small and fast

| | |
| --- | --- |
| Program | 4.3 MB, SQLite included |
| Memory | 4.4 MB idle, 9 MB with 3,000 tasks |
| Start | 3 ms |
| Board of 3,000 tasks | 17 ms to load in the browser |
| Agents' tool list | about 800 tokens, once per session |
| Agents' replies | 10 to 60 tokens of plain text |

Measured on an Apple M4.

## License

[AGPL-3.0](LICENSE): use it, change it and share it; if you offer a changed version over a network, share its source too. To help, see [CONTRIBUTING](.github/CONTRIBUTING.md).
