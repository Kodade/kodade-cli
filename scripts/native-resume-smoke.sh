#!/usr/bin/env bash
set -euo pipefail

# Exercises exact Codex resumes through real daemon PTYs; the fake executable
# records argv/PID, so terminal text cannot make this pass.
bin=${1:?usage: native-resume-smoke.sh /path/to/kodade-cli}
root=$(mktemp -d)
trap 'pkill -P $$ 2>/dev/null || true; rm -rf "$root"' EXIT
export HOME="$root/home" XDG_RUNTIME_DIR="$root/runtime" XDG_STATE_HOME="$root/state"
export PATH="$root/bin:$PATH" ARGS_LOG="$root/argv.log"
mkdir -p "$HOME/.config/kodade-cli" "$XDG_RUNTIME_DIR" "$root/bin" "$root/work"
printf '[session]\nresume_agents = true\n' >"$HOME/.config/kodade-cli/config.toml"
cat >"$root/bin/codex" <<'EOF'
#!/usr/bin/env bash
printf '%s:%s\n' "$$" "$*" >>"$ARGS_LOG"
exec sleep 60
EOF
chmod 700 "$root/bin/codex"
"$bin" -s native-smoke daemon >/dev/null 2>&1 &
sleep 1
p1=$(cd "$root/work" && "$bin" -s native-smoke run -- codex)
p2=$(cd "$root/work" && "$bin" -s native-smoke run -- codex)
printf '{"session_id":"first","nested":{"session_id":"decoy"}}' | "$bin" -s native-smoke agent report "$p1" working --source kodade:codex --native-agent codex --hook-json >/dev/null
printf '{"session_id":"second"}' | "$bin" -s native-smoke agent report "$p2" working --source kodade:codex --native-agent codex --hook-json >/dev/null
sleep 1
"$bin" -s native-smoke kill-session >/dev/null
: >"$ARGS_LOG"
"$bin" -s native-smoke daemon >/dev/null 2>&1 &
sleep 1
grep -Fx 'codex resume first' "$ARGS_LOG"
grep -Fx 'codex resume second' "$ARGS_LOG"
test "$(wc -l <"$ARGS_LOG")" -eq 2
echo 'native resume smoke passed'
