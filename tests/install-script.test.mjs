import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const sh = readFileSync(new URL('../install.sh', import.meta.url), 'utf8');
const ps1 = readFileSync(new URL('../install.ps1', import.meta.url), 'utf8');
assert.match(sh, /\n\{\nset -eu/);
assert.ok(sh.trimEnd().endsWith('}'));
assert.notEqual(ps1.charCodeAt(0), 0xFEFF, 'remote PowerShell scripts must not start with a BOM');
assert.match(ps1, /^[\x00-\x7F]*$/, 'BOM-less scripts must also parse in Windows PowerShell 5.1');
assert.doesNotMatch(ps1, /\bexit\s+[01]\b/, 'direct invocation must not close the terminal');
for (const source of [sh, ps1]) {
  assert.match(source, /polaris-agent/);
  assert.match(source, /record-installation/);
  assert.match(source, /command register/);
  assert.match(source, /command unregister/);
  assert.match(source, /POLARIS_AGENT_TOKEN/);
  assert.doesNotMatch(source, /uptime-probe|uptime-polaris-agent|UPTIME_AGENT|UPTIME_ENROLLMENT/);
  assert.match(source, /sha256 mismatch/);
  assert.match(source, /latest\/download\/manifest\.json/);
  assert.doesNotMatch(source, /\/api\/public\/agent\/(?:manifest|capabilities|download\/polaris-agent)/);
  assert.match(source, /configuration contains unsupported fields/);
}
assert.ok(sh.indexOf('fetch "$MANIFEST_URL"') < sh.indexOf('fetch "$URL"'));
assert.ok(ps1.indexOf('Invoke-RestMethod -Uri $manifestUrl') < ps1.indexOf('Invoke-WebRequest -Uri $url'));
assert.ok(sh.includes('URL="$RELEASE_BASE/download/$RELEASE_TAG/$RELEASE_FILE"'));
assert.ok(ps1.includes('$url = "$releaseBase/download/$($manifest.tag)/$releaseFile"'));
assert.match(sh, /--no-register-command/);
assert.match(ps1, /NoRegisterCommand/);
assert.ok(ps1.includes("[string]$Endpoint = ''"));
assert.doesNotMatch(ps1, /Read-Host "Endpoint/);
assert.ok(/nohup "\$DEST" run --config "\$CFG" >>"\$LOG_PATH"/.test(sh));
assert.match(sh, /command_args="run --config/);
assert.match(sh, /exec \$RUN --log-file/);
assert.match(sh, /ENTRYPOINT \[.*run/);
assert.ok(/die "[^"]*(?:manifest|required|verify)/.test(sh));
assert.ok(/Die "[^"]*(?:manifest|required|verify)/.test(ps1));
assert.match(ps1, /-Argument "run --config/);
assert.ok(sh.indexOf('sha256 mismatch') < sh.indexOf('mv -f "$BIN_NEW" "$DEST"'));
assert.ok(sh.indexOf('configuration contains unsupported fields') < sh.indexOf('mv -f "$BIN_NEW" "$DEST"'));

const temp = mkdtempSync(join(tmpdir(), 'polaris-installer-'));
const run = (program, args) => {
  const result = spawnSync(program, args, { encoding: 'utf8', windowsHide: true });
  assert.equal(result.status, 0, result.error?.message ?? result.stderr);
  return result;
};
try {
  const shPath = join(temp, 'install.sh');
  const psPath = join(temp, 'install.ps1');
  writeFileSync(shPath, sh);
  writeFileSync(psPath, ps1);
  run('sh', ['-n', shPath]);
  const openrcUnit = sh.match(/cat > "\/etc\/init\.d\/\$SERVICE_NAME" <<UNIT\n([^]*?)\nUNIT/)?.[1];
  assert.ok(openrcUnit);
  const openrcPath = join(temp, 'openrc-unit.sh');
  writeFileSync(openrcPath, `set -eu
SERVICE_MARKER='# fixture service'
AGENT_NAME=polaris-agent
SERVICE_NAME=polaris-agent
LOG_PATH=/var/log/polaris-agent.log
DEST=/usr/local/lib/polaris-agent/polaris-agent
CFG=/etc/polaris-agent/config.json
eval "$(cat <<UNIT
${openrcUnit}
UNIT
)"
printf '%s\\0' "$command" "$command_args" "$pidfile" "$retry" "$supervisor" "$respawn_max" "$respawn_delay" "\${command_background:-false}"
`);
  const [command, commandArgs, pidfile, retry, supervisor, respawnMax, respawnDelay, background] = run('sh', [openrcPath]).stdout.split('\0');
  assert.equal(command, '/usr/local/lib/polaris-agent/polaris-agent');
  assert.equal(commandArgs, 'run --config /etc/polaris-agent/config.json');
  assert.equal(pidfile, '/run/polaris-agent.pid');
  assert.equal(supervisor, 'supervise-daemon');
  assert.equal(respawnMax, '0', 'repeated exits must not exhaust supervisor retries');
  assert.equal(respawnDelay, '5');
  assert.equal(background, 'false', 'the supervisor must own the foreground Agent');
  const [stopSignal, graceSeconds, finalSignal, finalSeconds] = retry.split('/');
  assert.equal(stopSignal, 'TERM');
  assert.ok(Number(graceSeconds) > 20 + 5, 'allow an in-flight HTTP request and final report to finish');
  assert.equal(finalSignal, 'KILL');
  assert.ok(Number(finalSeconds) > 0, 'wait for forced termination before starting a replacement');
  const registration = sh.slice(sh.indexOf('scope="user"'), sh.indexOf('if [ "$REGISTER_COMMAND" = 1 ]; then'));
  assert.ok(registration.includes('record-installation'));
  const registrationPath = join(temp, 'registration.sh');
  writeFileSync(registrationPath, `set -eu
record_arguments() { printf '%s\\0' "$@"; }
die() { printf '%s\\n' "$*" >&2; exit 1; }
DEST=record_arguments
CFG='/etc/polaris-agent/config.json'
SERVICE_MANAGER=openrc
SERVICE_NAME=polaris-agent
MODE=system
IS_ROOT=1
INSTALL_DIR="$1"
LOG_PATH='/var/log/polaris-agent.log'
DOCKER_DIR='/tmp/polaris-agent-docker'
${registration}
`);
  for (const installDir of ['', '/opt/polaris custom']) {
    const argv = run('sh', [registrationPath, installDir]).stdout.split('\0').slice(0, -1);
    assert.equal(argv[0], 'record-installation');
    assert.ok(argv.every(value => value.length > 0), 'optional paths must not emit empty arguments');
    assert.equal(argv[argv.indexOf('--manager') + 1], 'openrc');
    assert.equal(argv[argv.indexOf('--config') + 1], '/etc/polaris-agent/config.json');
    assert.equal(argv[argv.indexOf('--scope') + 1], 'system');
    assert.equal(argv.includes('--install-dir'), Boolean(installDir));
    if (installDir) assert.equal(argv[argv.indexOf('--install-dir') + 1], installDir);
  }
  const parser = join(temp, 'parse.ps1');
  writeFileSync(parser, '$t=$null; $e=$null; [System.Management.Automation.Language.Parser]::ParseFile($args[0],[ref]$t,[ref]$e) > $null; if ($e.Count) { $e | Out-String | Write-Error; exit 1 }');
  run(process.platform === 'win32' ? 'powershell.exe' : 'pwsh', ['-NoProfile', '-NonInteractive', '-File', parser, psPath]);
  const merge = sh.match(/if ! EFFECTIVE_AUTO_UPDATE=\$\(python3 -[^]*?<<'PY'\n([^]*?)\nPY/)?.[1];
  assert.ok(merge);
  const mergePath = join(temp, 'merge.py');
  const saved = join(temp, 'saved.json');
  const update = join(temp, 'update.json');
  const merged = join(temp, 'merged.json');
  writeFileSync(mergePath, merge);
  const python = spawnSync('python3', ['--version'], { windowsHide: true }).status === 0 ? 'python3' : 'python';
  const args = [mergePath, saved, update, merged, '0', '0', '5', '60'];
  writeFileSync(saved, JSON.stringify({ endpoint: 'https://saved.test', token: 'saved-token', sample_interval: 10, report_interval: 90, include_nics: ['eth0'], insecure: true, auto_update: true, collect_gpus: false, collect_sensors: true, collect_containers: true, container_socket: 'http://127.0.0.1:2375' }));
  writeFileSync(update, JSON.stringify({ endpoint: 'https://new.test', token: 'new-token' }));
  assert.equal(run(python, args).stdout.trim(), '1');
  const config = JSON.parse(readFileSync(merged, 'utf8'));
  assert.equal(config.endpoint, 'https://saved.test');
  assert.equal(config.token, 'saved-token');
  assert.deepEqual(config.include_nics, ['eth0']);
  assert.equal(config.insecure, true);
  assert.equal(config.collect_gpus, false);
  assert.equal(config.collect_sensors, true);
  assert.equal(config.collect_containers, true);
  assert.equal(config.container_socket, 'http://127.0.0.1:2375');
  writeFileSync(update, JSON.stringify({ endpoint: 'https://new.test', token: 'new-token', auto_update: false }));
  assert.equal(run(python, args).stdout.trim(), '0');
  for (const bad of [{ ...config, interval: 30 }, { ...config, custom: true }, { ...config, sample_interval: 181, report_interval: 180 }, { ...config, collect_gpus: 'false' }, { ...config, collect_sensors: 0 }, { ...config, collect_containers: 'false' }, { ...config, container_socket: 5 }, { ...config, container_socket: '/tmp/bad\n' }]) {
    writeFileSync(saved, JSON.stringify(bad));
    assert.notEqual(spawnSync(python, args, { windowsHide: true }).status, 0);
  }
  writeFileSync(saved, '{broken json');
  assert.notEqual(spawnSync(python, args, { windowsHide: true }).status, 0);
} finally {
  rmSync(temp, { recursive: true, force: true });
}
console.log('PASS standalone installer sources, parsing and strict config merge');
