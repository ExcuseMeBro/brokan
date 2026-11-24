# Run Brokan on a server

One board on a VPS, at your own domain over HTTPS, used from every browser and by every agent at once. The board asks for an access token; agents send the same token.

You need a server with Docker, a domain whose A (and AAAA) record points at it, and ports 80 and 443 open.

## Start it

```sh
git clone https://github.com/ExcuseMeBro/brokan.git
cd brokan/deploy
cp .env.example .env
# In .env: BROKAN_DOMAIN=brokan.example.com and BROKAN_TOKEN=<the output of: openssl rand -hex 32>
docker compose up -d --build
```

Caddy gets a Let's Encrypt certificate for the domain and passes requests to the board. Open `https://brokan.example.com`, paste the token once, and the browser stays signed in for a year (an HttpOnly, Secure, SameSite=Strict cookie). `https://brokan.example.com/logout` signs that browser out.

The board lives in the `brokan-data` volume. To update, `git pull` and `docker compose up -d --build`; the board's own update button is off in the container.

## Connect your agents

The simplest way keeps every agent's existing `brokan mcp` setup. On each machine with agents, install Brokan and run once:

```sh
brokan connect https://brokan.example.com   # asks for the token, or reads $BROKAN_TOKEN
```

From then on `brokan mcp` on that machine relays to the server instead of the local board, for Claude Code, Codex, Pi or any MCP client that starts a command. It sends the git repos of the folder the agent works in, so a repo linked to a project still picks that project. If the server restarts, the next tool call opens a new session by itself. The URL and token are saved in `~/.config/brokan/remote.toml` (mode 0600), outside the synced board folder. `brokan disconnect` goes back to the local board, and `brokan mcp --local` uses the local board once.

Clients that speak MCP over HTTP can also connect directly, without a local Brokan:

```sh
# Claude Code (it stores the header, token included, in its own settings)
claude mcp add --transport http brokan https://brokan.example.com/mcp \
  --header "Authorization: Bearer $BROKAN_TOKEN"
```

```toml
# Codex, ~/.codex/config.toml: the token stays in the environment
[mcp_servers.brokan]
url = "https://brokan.example.com/mcp"
bearer_token_env_var = "BROKAN_TOKEN"
```

Over a direct HTTP connection the board can't see the agent's folder, so agents pass `project` by name.

## Settings for a server

These are per machine, set in the environment (or as flags), never in `config.toml`, which sync shares with every machine:

| Variable | Flag | Does |
| --- | --- | --- |
| `BROKAN_PUBLIC_URL` | `--public-url` | The address the board is reached at, like `https://brokan.example.com`. The board accepts that Host and Origin, and requires a token |
| `BROKAN_TOKEN` | | The access token: at least 24 characters of letters, digits and `-_.~+/=` |
| `BROKAN_TOKEN_FILE` | | A file holding the token instead, like a Docker or systemd secret |
| `BROKAN_LISTEN` | `--listen` | The address to listen on; `0.0.0.0` in the container, `127.0.0.1` behind a proxy on the same host |
| `BROKAN_PORT` | `--port` | The port, 1317 by default |
| `BROKAN_UPDATES=off` | | Never offer or install updates from the board |

A token works without a public URL too, for example on a home network: every request then needs it.

## Without Docker

Install Brokan, then use [`deploy/brokan.service`](../deploy/brokan.service) as a systemd unit with the variables in `/etc/brokan.env`. Put any reverse proxy with TLS in front of `127.0.0.1:1317`. It must keep the original `Host` header and must not buffer server-sent events. For nginx:

```nginx
server {
    listen 443 ssl http2;
    server_name brokan.example.com;
    # ssl_certificate ... (certbot)

    location / {
        proxy_pass http://127.0.0.1:1317;
        proxy_set_header Host $host;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_buffering off;          # MCP streams its answers
        proxy_read_timeout 1h;
        client_max_body_size 25m;     # pasted images
    }
}
```

## Sync and backups

The server's board can sync with a private git repo like any other machine: mount a folder with an SSH deploy key and a `known_hosts` entry for the git host (see the comment in `docker-compose.yml`) and set the repo in Settings → Sync. Laptops that keep their own boards can sync with the same repo; when both changed, sync merges them. A copy of the `brokan-data` volume, or of `brokan.db` while the board is stopped, is a full backup.
