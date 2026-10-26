<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/kanzo-dark.jpg">
    <img alt="kanzo: a tiny kanban board for you and your coding agents" src="docs/img/kanzo-light.jpg">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml"><img src="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/rust-1.89+-orange?logo=rust" alt="Rust 1.89+">
</p>

Kanzo is one small program. It shows a kanban board in your browser, and it is an MCP server, so Claude Code, Codex and other agents work on the same tasks you see. Everything is in one folder.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/board-dark.jpg">
  <img alt="The Kanzo board: a project with todo, doing, review and done columns" src="docs/img/board-light.jpg">
</picture>

## Why Kanzo

- **Simple.** Projects, tasks, four columns, Markdown and comments. Nothing to configure.
- **Light.** One 4 MB file with SQLite inside. No Docker, no Node, no database server. It runs on a Raspberry Pi Zero.
- **Made for agents.** Agents can do everything you can do on the board, and they find the right project from the repo they work in.
- **Yours.** The board is one folder: copy it, back it up, or sync it through a private git repo.

## Get started

```sh
curl -LsSf https://github.com/emanuele-em/kanzo/releases/latest/download/kanzo-installer.sh | sh

kanzo                                            # the board, at http://localhost:4242
claude mcp add kanzo --scope user -- kanzo mcp   # connect Claude Code
codex mcp add kanzo -- kanzo mcp                 # connect Codex
```

Then ask your agent: *"take the next task from kanzo and do it."* It moves the card to `doing` and comments as it works.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/task-dark.jpg">
  <img alt="A task open on the board, with a description and comments from Claude Code" src="docs/img/task-light.jpg">
</picture>

## Light by design

| What | Measured |
| --- | --- |
| Program size | 4.3 MB, SQLite included |
| Memory when idle | about 5 MB |
| Threads when idle | 1 |
| Ready after launch | 4 ms for the board, 10 ms for `kanzo mcp` |
| Loading a board of 2,000 tasks | 3 ms |
| Checking for changes, every 2 s | under 1 ms, and no disk writes |

Measured on an Apple M4. In a Linux container limited to 64 MB of memory and half a CPU, the ARM64 build idles at 3.5 MB.

It runs on macOS 10.12 or later, and on any 64-bit or ARM Linux, from a Raspberry Pi Zero to a Pi 5. The board works in browsers back to Safari 13.1, Chrome 80 and Firefox 78.

## Learn more

The [guide](docs/guide.md) covers running Kanzo in the background, opening it from other devices, MCP over HTTP, syncing between laptops, export and settings.
