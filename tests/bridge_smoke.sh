#!/bin/sh
# Proves a board on a server end to end: it refuses requests without its token, and
# `brokan mcp` relays an agent to it (with the agent's repo picking the project) and
# opens a new session by itself after the board restarts. Needs sqlite3 and curl.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${1:-$root/target/debug/brokan}"
[ -x "$bin" ] || { echo "missing $bin; run cargo build first" >&2; exit 1; }
tmp=$(mktemp -d)
port=$(( 20000 + $$ % 20000 ))
export BROKAN_TOKEN="smoke-token-smoke-token-1234"
start() { "$bin" --foreground --no-open --home "$tmp/board" --port "$port" --listen 127.0.0.1 --public-url "http://127.0.0.1:$port" >>"$tmp/server.log" 2>&1 3>&- </dev/null & echo $! > "$tmp/pid"; 
  i=0; until curl -fsS "http://127.0.0.1:$port/api/ping" >/dev/null 2>&1; do i=$((i+1)); [ $i -gt 100 ] && { cat "$tmp/server.log"; exit 1; }; sleep 0.1; done; }
stop() { kill "$(cat "$tmp/pid")"; wait "$(cat "$tmp/pid")" 2>/dev/null || true; }
trap 'stop 2>/dev/null; rm -rf "$tmp"' EXIT
start
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/api/board")
[ "$code" = 401 ] || { echo "FAIL: board without a token answered $code"; exit 1; }
mkdir -p "$tmp/repo" && git -C "$tmp/repo" init -q && git -C "$tmp/repo" remote add origin git@github.com:acme/widgets.git
fifo="$tmp/in"; mkfifo "$fifo"
( cd "$tmp/repo" && BROKAN_URL="http://127.0.0.1:$port" "$bin" mcp <"$fifo" >"$tmp/out" 2>"$tmp/err" ) &
bridge=$!
exec 3>"$fifo"
send() { printf '%s\n' "$1" >&3; }
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'
send '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_project","arguments":{"slug":"widgets"}}}'
sleep 1
# Link the repo so project detection over the bridge can be checked.
sqlite3 "$tmp/board/brokan.db" "INSERT INTO project_repos VALUES ('acme/widgets', (SELECT id FROM projects WHERE slug = 'widgets'))" \
  || { cat "$tmp/out" "$tmp/err" "$tmp/server.log"; exit 1; }
send '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_task","arguments":{"title":"from the bridge"}}}'
sleep 1
stop; start   # the board forgets every session
send '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_tasks","arguments":{"project":"widgets"}}}'
sleep 2
exec 3>&-
wait $bridge || true
check() { grep -q "$1" "$tmp/out" || { echo "FAIL: $2"; cat "$tmp/out" "$tmp/err"; exit 1; }; }
check '"id":1,.*protocolVersion' "no answer to initialize"
check '"id":2,.*Created project widgets' "create_project failed"
check '"id":3,.*Created widgets#1' "the agent's repo did not pick the project"
check '"id":4,.*widgets#1 \[todo\] from the bridge' "no new session after the board restarted"
echo PASS
