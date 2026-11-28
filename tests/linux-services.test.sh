#!/bin/sh
set -eu

[ "$(id -u)" = 0 ] || { echo 'Service integration test requires root' >&2; exit 1; }
command -v systemctl >/dev/null
command -v s6-supervise >/dev/null
agent="$(pwd)/target/debug/polaris-agent"
[ -x "$agent" ]
fixture=$(mktemp -d /var/lib/polaris-service-test.XXXXXX)
SERVICE_NAME="polaris-service-test-$$"
SERVICE_MARKER="# polaris-agent-managed:$SERVICE_NAME:"
BIN_DIR="$fixture/install dir/bin"
CFG_DIR="$fixture/install dir/config"
DEST="$BIN_DIR/agent"
CFG="$CFG_DIR/config.json"
LOG_PATH="$CFG_DIR/output.log"
unit="/run/systemd/system/$SERVICE_NAME.service"
live="/run/service/$SERVICE_NAME"
supervisor_pid=''
[ ! -e "$unit" ] && [ ! -e "$live" ]
cleanup() {
  systemctl stop "$SERVICE_NAME" >/dev/null 2>&1 || true
  rm -f "$unit"
  systemctl daemon-reload
  if [ -n "$supervisor_pid" ]; then
    s6-svc -dx "$live" >/dev/null 2>&1 || true
    wait "$supervisor_pid" || true
  fi
  rm -rf "$live" "$fixture"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
mkdir -p "$BIN_DIR" "$CFG_DIR"
printf '{}\n' > "$CFG"
cat > "$DEST" <<'AGENT'
#!/bin/sh
set -eu
[ "$1" = run ] && [ "$2" = --config ] && [ -f "$3" ]
config_dir=$(dirname "$3")
printf '{}\n' > "$3.next"
mv "$3.next" "$3"
cp "$0" "$0.next"
ln "$0" "$0.backup"
chmod 755 "$0.next"
mv "$0.next" "$0"
rm "$0.backup"
if [ "$#" = 3 ]; then
  outside="$config_dir/../../outside"
  if (printf 'unexpected\n' > "$outside") 2>/dev/null; then exit 71; fi
else
  [ "$#" = 5 ] && [ "$4" = --log-file ] && [ "$5" = "$config_dir/output.log" ]
fi
printf '%s\n' "$$" >> "$config_dir/runs"
trap 'exit 0' TERM INT
while :; do sleep 1; done
AGENT
chmod 755 "$DEST"
wait_runs() {
  attempts=0
  while [ ! -f "$CFG_DIR/runs" ] || [ "$(wc -l < "$CFG_DIR/runs")" -lt "$1" ]; do
    attempts=$((attempts + 1))
    if [ "$attempts" -gt 20 ]; then
      journalctl -u "$SERVICE_NAME" --no-pager -n 30
      [ ! -f "$LOG_PATH" ] || cat "$LOG_PATH"
      echo 'Service did not start or respawn' >&2
      exit 1
    fi
    sleep 1
  done
}

setup=$(sed -n '/^RUN=/,/^# ---/p' install.sh | sed '$d')
eval "$setup"
check_unit_paths
template=$(awk '/^  cat > "\/etc\/systemd\/system\/\$SERVICE_NAME.service" <<UNIT$/ { found=1; next } found && /^UNIT$/ { exit } found { print }' install.sh)
[ -n "$template" ]
eval "cat > \"\$unit\" <<UNIT
$template
UNIT"
systemd-analyze verify "$unit"
systemctl daemon-reload
systemctl start "$SERVICE_NAME"
wait_runs 1
systemctl is-active "$SERVICE_NAME"
systemctl kill --kill-whom=main --signal=TERM "$SERVICE_NAME"
wait_runs 2
systemctl stop "$SERVICE_NAME"
[ ! -f "$fixture/outside" ]
echo 'PASS real systemd: spaced paths, atomic config/binary writes, sandbox and clean-exit restart'

rm "$CFG_DIR/runs"
mkdir -p "$live"
template=$(awk '/^  cat > "\/etc\/services.d\/\$SERVICE_NAME\/run" <<UNIT$/ { found=1; next } found && /^UNIT$/ { exit } found { print }' install.sh)
[ -n "$template" ]
eval "cat > \"\$live/run\" <<UNIT
$template
UNIT"
chmod 755 "$live/run"
s6-supervise "$live" > "$LOG_PATH" 2>&1 &
supervisor_pid=$!
wait_runs 1
"$agent" record-installation --config "$CFG" --manager s6 --service-name "$SERVICE_NAME" --scope system --mode system --log-path "$LOG_PATH"
"$agent" status --config "$CFG" | grep '^up '
"$agent" restart --config "$CFG"
wait_runs 2
"$agent" stop --config "$CFG"
s6-svwait -d -t 10000 "$live"
"$agent" status --config "$CFG" | grep '^down '
"$agent" start --config "$CFG"
wait_runs 3
"$agent" stop --config "$CFG"
s6-svwait -d -t 10000 "$live"
echo 'PASS real s6: executable run script and native Agent start/stop/restart/status'
