#!/usr/bin/env bash
# End-to-end local extension fixture: no network or user configuration needed.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin=${1:-"$root/target/debug/kodade-cli"}
tmp=$(mktemp -d)
cleanup() {
  HOME="$tmp/home" XDG_RUNTIME_DIR="$tmp/runtime" "$bin" --session plugins kill-session >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT
mkdir -p "$tmp/home" "$tmp/runtime"
export HOME="$tmp/home" XDG_RUNTIME_DIR="$tmp/runtime" XDG_STATE_HOME="$tmp/state"

"$bin" --session plugins plugin link "$root/examples/plugins/hello" >/dev/null
"$bin" --session plugins new --workspace plugins "$tmp/workspace" >/dev/null
for _ in $(seq 1 20); do
  grep -q 'started plugins' "$HOME/.config/kodade-cli/plugins/logs/hello-plugin.log" 2>/dev/null && break
  sleep 0.1
done
grep -q 'started plugins' "$HOME/.config/kodade-cli/plugins/logs/hello-plugin.log"
"$bin" --session plugins plugin list --json | grep -q '"hello-plugin"'
"$bin" --session plugins plugin run hello-plugin hello | grep -q 'hello from plugins'
"$bin" --session plugins plugin pane hello-plugin monitor >/dev/null
sleep 0.2
"$bin" --session plugins plugin disable hello-plugin >/dev/null
log="$HOME/.config/kodade-cli/plugins/logs/hello-plugin.log"
before=$(wc -c < "$log")
"$bin" --session plugins new-tab >/dev/null
sleep 0.2
test "$before" = "$(wc -c < "$log")"
if "$bin" --session plugins plugin run hello-plugin hello >/dev/null 2>&1; then
  echo "disabled plugin ran" >&2
  exit 1
fi
"$bin" --session plugins plugin enable hello-plugin >/dev/null
"$bin" --session plugins plugin unlink hello-plugin >/dev/null
test ! -e "$HOME/.config/kodade-cli/plugins/managed/hello-plugin"

echo "plugin smoke passed"
