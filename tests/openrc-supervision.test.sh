#!/bin/sh
set -eu

[ "$(id -u)" = 0 ] || { echo 'OpenRC integration test requires root' >&2; exit 1; }
command -v supervise-daemon >/dev/null
SERVICE_NAME="polaris-supervision-test-$$"
AGENT_NAME=polaris-agent
SERVICE_MARKER="# polaris-agent-managed:$SERVICE_NAME:"
fixture=$(mktemp -d)
DEST="$fixture/agent"
CFG="$fixture/config file.json"
LOG_PATH="$fixture/output.log"
unit="/etc/init.d/$SERVICE_NAME"
[ ! -e "$unit" ] || exit 1
cleanup() {
  rc-service "$SERVICE_NAME" stop >/dev/null 2>&1 || true
  rc-service "$SERVICE_NAME" zap >/dev/null 2>&1 || true
  rm -f "$unit" "/run/$SERVICE_NAME.pid"
  rm -rf "$fixture"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
write_agent() {
  cat > "$DEST.next" <<EOF
#!/bin/sh
[ "\$#" = 3 ] && [ "\$1" = run ] && [ "\$2" = --config ] && [ "\$3" = '$CFG' ] || exit 70
trap 'exit 0' TERM INT
printf '%s %s\n' "\$\$" '$1' >> '$fixture/runs'
while :; do sleep 1; done
EOF
  chmod 755 "$DEST.next"
  mv "$DEST.next" "$DEST"
}
wait_runs() {
  attempts=0
  while [ ! -f "$fixture/runs" ] || [ "$(wc -l < "$fixture/runs")" -lt "$1" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -le 20 ] || { cat "$LOG_PATH"; echo 'Agent was not respawned' >&2; exit 1; }
    sleep 1
  done
}
child_pid() { tail -n 1 "$fixture/runs" | cut -d ' ' -f 1; }

write_agent first
template=$(awk '/^  cat > "\/etc\/init.d\/\$SERVICE_NAME" <<UNIT$/ { found=1; next } found && /^UNIT$/ { exit } found { print }' install.sh)
[ -n "$template" ]
render_unit() {
  eval "cat > \"\$fixture/new-unit\" <<UNIT
$template
UNIT"
  printf '\ndepend() { :; }\n' >> "$fixture/new-unit"
}
render_unit
sed 's/supervisor="supervise-daemon"/command_background=true/; /^respawn_max=/d; /^command=/s/\\"//g' "$fixture/new-unit" > "$unit"
chmod 755 "$unit"
rc-service "$SERVICE_NAME" start
wait_runs 1
kill -TERM "$(child_pid)"
sleep 2
rc-service "$SERVICE_NAME" stop

DEST="$fixture/agent binary"
write_agent replacement
render_unit
cp "$fixture/new-unit" "$unit"
chmod 755 "$unit"
rc-service "$SERVICE_NAME" start
wait_runs 2
supervisor_pid=$(cat "/run/$SERVICE_NAME.pid")
write_agent updated
kill -TERM "$(child_pid)"
wait_runs 3
[ "$(tail -n 1 "$fixture/runs" | cut -d ' ' -f 2)" = updated ]
[ "$(cat "/run/$SERVICE_NAME.pid")" = "$supervisor_pid" ]
kill -KILL "$(child_pid)"
wait_runs 4
rc-service "$SERVICE_NAME" status
last_child=$(child_pid)
rc-service "$SERVICE_NAME" stop
sleep 6
[ "$(wc -l < "$fixture/runs")" -eq 4 ]
! kill -0 "$last_child" 2>/dev/null
! kill -0 "$supervisor_pid" 2>/dev/null
echo 'PASS real OpenRC: crashed-service reinstall, binary replacement, clean exit, crash restart and explicit stop'
