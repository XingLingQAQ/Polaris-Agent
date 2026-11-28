import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const installer = readFileSync(new URL('../install.sh', import.meta.url), 'utf8');
const start = installer.indexOf('elif [ "$HAVE_OPENRC" = 1 ] && [ "$IS_ROOT" = 1 ]; then\n');
const end = installer.indexOf('\nelif [ "$HAVE_SUPERVISORD"', start);
assert.ok(start > 0 && end > start);
const branch = installer.slice(installer.indexOf('\n', start) + 1, end)
  .replaceAll('/etc/init.d/', '$FIXTURE_INIT_DIR/');
const preflight = installer.match(/if \[ "\$HAVE_SYSTEMD" = 0 \] && \[ "\$HAVE_OPENRC" = 1 \][^]*?\nfi/)?.[0];
assert.ok(preflight);
assert.ok(installer.indexOf(preflight) < installer.indexOf('fetch "$MANIFEST_URL"'));
const temp = mkdtempSync(join(tmpdir(), 'polaris-openrc-'));
const run = (file, env) => spawnSync('sh', [file], { encoding: 'utf8', windowsHide: true, timeout: 15_000, env: { ...process.env, ...env } });

try {
  const check = join(temp, 'preflight.sh');
  writeFileSync(check, `set -eu
die() { printf '%s\\n' "$*" >&2; exit 1; }
command() { if [ "$*" = '-v supervise-daemon' ]; then return 1; fi; command command "$@"; }
${preflight}
`);
  for (const mode of ['auto', 'system', 'manual', 'docker', 'user']) {
    const result = run(check, { HAVE_SYSTEMD: '0', HAVE_OPENRC: '1', IS_ROOT: '1', MODE: mode });
    assert.equal(result.status, ['auto', 'system'].includes(mode) ? 1 : 0, result.stderr);
  }
  assert.equal(run(check, { HAVE_SYSTEMD: '1', HAVE_OPENRC: '1', IS_ROOT: '1', MODE: 'system' }).status, 0);

  for (const scenario of ['fresh', 'running', 'crashed', 'supervised', 'stop-failed', 'start-failed', 'status-failed', 'foreign']) {
    const root = join(temp, scenario);
    mkdirSync(join(root, 'init'), { recursive: true });
    mkdirSync(join(root, 'bin'));
    const service = join(root, 'init', 'fixture-custom');
    const marker = '# polaris-agent-managed:fixture-custom:';
    const old = scenario === 'foreign' ? '# another service\n' : `${marker}\n${scenario === 'supervised' ? 'supervisor="supervise-daemon"' : 'command_background=true'}\n`;
    if (scenario !== 'fresh') writeFileSync(service, old);
    const serviceLog = join(root, 'calls');
    writeFileSync(join(root, 'bin', 'rc-service'), `#!/bin/sh
set -eu
printf '%s\\n' "$2" >> "$FIXTURE_CALLS"
[ "$1" = fixture-custom ] || exit 88
case "$2" in
  stop)
    cmp "$FIXTURE_OLD" "$FIXTURE_INIT_DIR/$1" || exit 89
    [ "$FIXTURE_SCENARIO" != stop-failed ] || exit 1 ;;
  start)
    [ "$(cat "$FIXTURE_TIER")" = A ] || exit 90
    . "$FIXTURE_INIT_DIR/$1"
    [ "$supervisor" = supervise-daemon ] && [ "$respawn_max" = 0 ] && [ "$respawn_delay" = 5 ] || exit 91
    [ "\${command_background:-false}" = false ] || exit 92
    [ "$pidfile" = /run/fixture-custom.pid ] && [ "$output_log" = /var/log/fixture-custom.log ] && [ "$error_log" = "$output_log" ] || exit 93
    eval "set -- $command"
    [ "$#" = 1 ] && [ "$1" = '/opt/polaris agent/polaris-agent' ] || exit 96
    eval "set -- $command_args"
    [ "$#" = 3 ] && [ "$1" = run ] && [ "$2" = --config ] && [ "$3" = '/opt/polaris agent/config.json' ] || exit 95
    [ "$FIXTURE_SCENARIO" != start-failed ] || exit 1 ;;
  status) [ "$FIXTURE_SCENARIO" != status-failed ] || exit 32 ;;
  *) exit 94 ;;
esac
`, { mode: 0o755 });
    writeFileSync(join(root, 'bin', 'rc-update'), '#!/bin/sh\nprintf "enable\\n" >> "$FIXTURE_CALLS"\n', { mode: 0o755 });
    writeFileSync(join(root, 'old'), old);
    const runner = join(root, 'run.sh');
    writeFileSync(runner, `set -eu
cd "$FIXTURE_ROOT"
FIXTURE_INIT_DIR="$PWD/init"
FIXTURE_CALLS="$PWD/calls"
FIXTURE_OLD="$PWD/old"
FIXTURE_TIER="$PWD/tier"
export FIXTURE_INIT_DIR FIXTURE_CALLS FIXTURE_OLD FIXTURE_TIER
PATH="$PWD/bin:$PATH"
AGENT_NAME=polaris-agent
SERVICE_NAME=fixture-custom
SERVICE_MARKER='${marker}'
DEST='/opt/polaris agent/polaris-agent'
CFG='/opt/polaris agent/config.json'
die() { printf '%s\\n' "$*" >&2; exit 1; }
set_service_tier() { printf '%s' "$1" > "$FIXTURE_TIER"; }
${branch}
printf '%s\\n' "$TIER" "$NOTE"
`);
    const result = run(runner, { FIXTURE_ROOT: root, FIXTURE_SCENARIO: scenario });
    const calls = existsSync(serviceLog) ? readFileSync(serviceLog, 'utf8').trim().split('\n') : [];
    if (scenario === 'foreign' || scenario === 'stop-failed') {
      assert.notEqual(result.status, 0);
      assert.equal(readFileSync(service, 'utf8'), old);
      assert.deepEqual(calls, scenario === 'foreign' ? [] : ['stop']);
    } else if (scenario === 'start-failed' || scenario === 'status-failed') {
      assert.notEqual(result.status, 0);
      assert.doesNotMatch(result.stdout, /starts it on boot/);
      assert.match(result.stderr, scenario === 'start-failed' ? /cannot start OpenRC/ : /did not stay active/);
    } else {
      assert.equal(result.status, 0, `${scenario}: ${result.stdout}\n${result.stderr}`);
      assert.deepEqual(calls, [...(scenario === 'fresh' ? [] : ['stop']), 'enable', 'start', 'status']);
      assert.match(result.stdout, /^A\nOpenRC supervises/);
    }
  }
} finally {
  rmSync(temp, { recursive: true, force: true });
}
console.log('PASS OpenRC installer: supervision, reinstall ordering, crashed service, isolated names and failure handling');
