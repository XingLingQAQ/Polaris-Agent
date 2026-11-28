import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const installer = readFileSync(new URL('../install.sh', import.meta.url), 'utf8');
const setup = installer.slice(installer.indexOf('RUN='), installer.indexOf('\n# ---', installer.indexOf('RUN=')));
const branch = (condition) => {
  const start = installer.indexOf(`elif ${condition}; then\n`);
  assert.ok(start > 0, condition);
  return installer.slice(installer.indexOf('\n', start) + 1, installer.indexOf('\nelif ', start))
    .replaceAll('/etc/systemd/system/', '$FIXTURE_ROOT/units/')
    .replaceAll('/etc/supervisor/conf.d', '$FIXTURE_ROOT/supervisor')
    .replaceAll('/etc/services.d/', '$FIXTURE_ROOT/definitions/')
    .replaceAll('/run/service/', '$FIXTURE_ROOT/live/');
};
const branches = {
  system: branch('[ "$HAVE_SYSTEMD" = 1 ] && [ "$IS_ROOT" = 1 ]'),
  user: branch('[ "$MODE" = "user" ] && [ "$HAVE_SYSTEMD" = 1 ]'),
  supervisor: branch('[ "$HAVE_SUPERVISORD" = 1 ] && [ "$IS_ROOT" = 1 ]'),
  s6: branch('[ "$HAVE_S6" = 1 ] && [ "$IS_ROOT" = 1 ]'),
};
const temp = mkdtempSync(join(tmpdir(), 'polaris-services-'));
const run = (file, env = {}) => spawnSync('sh', [file], {
  encoding: 'utf8', timeout: 15_000, windowsHide: true, env: { ...process.env, ...env },
});

try {
  for (const scenario of ['system-off', 'system-on', 'user', 'supervisor', 's6-fresh', 's6-live', 's6-foreign']) {
    const root = join(temp, scenario);
    mkdirSync(join(root, 'units'), { recursive: true });
    const fixture = join(root, 'fixture.sh');
    writeFileSync(fixture, `set -eu
cd "$FIXTURE_NATIVE_ROOT"
FIXTURE_ROOT="$PWD"
BIN_DIR="$PWD/install dir/bin"
CFG_DIR="$PWD/install dir/config"
DEST="$BIN_DIR/agent"
CFG="$CFG_DIR/config.json"
LOG_PATH="$CFG_DIR/agent.log"
SERVICE_NAME=fixture-custom
SERVICE_MARKER='# polaris-agent-managed:fixture-custom:'
EFFECTIVE_AUTO_UPDATE=${scenario === 'system-on' ? '1' : '0'}
XDG_CONFIG_HOME="$PWD/user config"
mkdir -p "$BIN_DIR" "$CFG_DIR"
cat > "$DEST" <<'AGENT'
#!/bin/sh
printf '%s\\n' "$@" > "$FIXTURE_ROOT/argv"
AGENT
chmod 755 "$DEST"
export FIXTURE_ROOT
die() { printf '%s\\n' "$*" >&2; exit 1; }
warn() { printf '%s\\n' "$*" >&2; }
set_service_tier() { printf '%s' "$1" > "$FIXTURE_ROOT/tier"; }
systemctl() { printf '%s\\n' "$*" >> "$FIXTURE_ROOT/calls"; }
supervisorctl() { printf '%s\\n' "$*" >> "$FIXTURE_ROOT/calls"; }
loginctl() { printf 'Linger=yes\\n'; }
mkdir -p "$PWD/stubs"
cat > "$PWD/stubs/s6-svc" <<'STUB'
#!/bin/sh
set -eu
cmp "$FIXTURE_ROOT/definitions/fixture-custom/run" "$2/run"
printf '%s\\n' "$*" >> "$FIXTURE_ROOT/calls"
STUB
chmod 755 "$PWD/stubs/s6-svc"
PATH="$PWD/stubs:$PATH"
${setup}
${scenario.startsWith('s6-') && scenario !== 's6-fresh' ? `mkdir -p "$FIXTURE_ROOT/live/$SERVICE_NAME"
printf '%s\\n' '${scenario === 's6-foreign' ? '# another service' : '# polaris-agent-managed:fixture-custom:'}' '#!/bin/sh' 'exit 1' > "$FIXTURE_ROOT/live/$SERVICE_NAME/run"` : ''}
${branches[scenario.startsWith('system-') ? 'system' : scenario.startsWith('s6-') ? 's6' : scenario]}
printf '%s\\n' "$FIXTURE_ROOT" > "$FIXTURE_ROOT/posix-root"
`);
    const result = run(fixture, { FIXTURE_NATIVE_ROOT: root });
    if (scenario === 's6-foreign') {
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /refusing to overwrite existing live s6 service/);
      assert.equal(existsSync(join(root, 'calls')), false);
      assert.match(readFileSync(join(root, 'live/fixture-custom/run'), 'utf8'), /^# another service/);
      continue;
    }
    assert.equal(result.status, 0, `${scenario}: ${result.stdout}\n${result.stderr}`);
    assert.equal(readFileSync(join(root, 'tier'), 'utf8'), 'A');
    const posixRoot = readFileSync(join(root, 'posix-root'), 'utf8').trim();
    let command;
    if (scenario.startsWith('system-') || scenario === 'user') {
      const unit = readFileSync(join(root, scenario === 'user' ? 'user config/systemd/user/fixture-custom.service' : 'units/fixture-custom.service'), 'utf8');
      command = unit.match(/^ExecStart=(.+)$/m)?.[1];
      assert.ok(command);
      if (scenario !== 'user') {
        assert.ok(unit.includes(`ReadWritePaths=/run "${posixRoot}/install dir/bin" "${posixRoot}/install dir/config"`));
        assert.match(unit, /^ProtectSystem=strict$/m);
        assert.match(unit, /^ProtectHome=true$/m);
      }
      assert.match(readFileSync(join(root, 'calls'), 'utf8'), /restart fixture-custom/);
    } else if (scenario === 'supervisor') {
      command = readFileSync(join(root, 'supervisor/fixture-custom.conf'), 'utf8').match(/^command=(.+)$/m)?.[1];
      assert.ok(command);
    } else {
      const script = readFileSync(join(root, 'definitions/fixture-custom/run'), 'utf8');
      assert.ok(script.startsWith('#!/bin/sh\n'));
      command = `"${posixRoot}/definitions/fixture-custom/run"`;
      if (scenario === 's6-fresh') assert.match(result.stderr, /restart the container/);
      else assert.equal(readFileSync(join(root, 'live/fixture-custom/run'), 'utf8'), script);
    }
    const launch = join(root, 'launch.sh');
    writeFileSync(launch, `set -eu\nexport FIXTURE_ROOT='${posixRoot}'\n${command}\n`);
    const launched = run(launch);
    assert.equal(launched.status, 0, launched.stderr);
    assert.deepEqual(readFileSync(join(root, 'argv'), 'utf8').trim().split('\n'), [
      'run', '--config', `${posixRoot}/install dir/config/config.json`,
      ...(scenario.startsWith('s6-') ? ['--log-file', `${posixRoot}/install dir/config/agent.log`] : []),
    ]);
  }
  const check = join(temp, 'check.sh');
  writeFileSync(check, `set -eu\ndie() { printf '%s\\n' "$*" >&2; exit 1; }\n${setup}\ncheck_unit_paths\nprintf 'accepted\\n'\n`);
  for (const path of ['/opt/polaris-agent', '/opt/polaris agent']) {
    assert.equal(run(check, { DEST: `${path}/bin/agent`, CFG: `${path}/config.json` }).stdout, 'accepted\n');
  }
  for (const path of ['/opt/bad%name', '/opt/bad"name', '/opt/bad$name']) {
    const result = run(check, { DEST: `${path}/agent`, CFG: '/etc/polaris-agent/config.json' });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /unsupported by systemd/);
  }
} finally {
  rmSync(temp, { recursive: true, force: true });
}
console.log('PASS service installers: system/user systemd, writable update directories, spaced arguments, supervisord and live s6 replacement');
