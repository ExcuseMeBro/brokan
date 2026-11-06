#!/usr/bin/env bash
# Proves the automatic sync timer end to end against a real board and a real remote:
# configure a remote, turn automatic sync on, wait for the timer to fire, and check the
# remote actually received the commit. Needs a debug build (it shortens the interval
# through BROKAN_AUTO_SYNC_SECS, which release builds ignore).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/debug/brokan"
[ -x "$bin" ] || { echo "missing $bin; run cargo build first" >&2; exit 1; }

tmp="$(mktemp -d)"
port="${BROKAN_SMOKE_PORT:-8791}"
pid=""
cleanup() {
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT

git init --bare -q "$tmp/remote.git"

start_board() {
  BROKAN_HOME="$tmp/board" BROKAN_AUTO_SYNC_SECS=2 "$bin" --foreground --no-open --port "$port" >"$tmp/board.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 100); do
    if curl -fsS "http://127.0.0.1:$port/api/ping" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  echo "board did not start; log:" >&2
  cat "$tmp/board.log" >&2
  return 1
}

api() { curl -fsS -X "$1" -H 'content-type: application/json' -d "${3:-{\}}" "http://127.0.0.1:$port$2"; }

start_board
api PUT /api/sync "{\"url\":\"$tmp/remote.git\"}" >/dev/null
api PUT /api/sync/automatic '{"enabled":true}' >/dev/null

status="$(api GET /api/sync)"
echo "status: $status"
case "$status" in
  *'"automatic":true'*) ;;
  *) echo "automatic sync did not report enabled" >&2; exit 1 ;;
esac

api POST /api/projects '{"slug":"smoke","name":"Smoke"}' >/dev/null

for _ in $(seq 1 60); do
  if [ "$(git --git-dir="$tmp/remote.git" rev-list --count refs/heads/* 2>/dev/null | head -1)" != "" ]; then break; fi
  sleep 0.5
done

count="$(git --git-dir="$tmp/remote.git" rev-list --count --all)"
echo "remote commits: $count"
[ "$count" -ge 1 ] || { echo "timer never pushed to the remote" >&2; exit 1; }
git --git-dir="$tmp/remote.git" log --oneline --all | head -3

kill "$pid"; wait "$pid" 2>/dev/null || true; pid=""
start_board
status="$(api GET /api/sync)"
echo "after restart: $status"
case "$status" in
  *'"automatic":true'*) echo "PASS" ;;
  *) echo "automatic sync was not remembered across a restart" >&2; exit 1 ;;
esac
