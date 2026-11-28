# polaris-agent

[English](README.md) · [简体中文](README-zh-CN.md)

`polaris-agent` is the host agent for [Uptime Polaris](https://github.com/XingLingQAQ/UptimePolaris). It reports CPU, memory, disk, network and load, and runs network probes from the machine it is installed on. `pa` is the short command; both operate the same program, configuration and service.

## Install

1. Enable the machine page under **Settings → Presentation** in Uptime Polaris, then create a machine in the Agent console.
2. Choose the platform, installation mode and intervals.
3. Copy the generated command from the production origin.
4. Run it on the target machine with the permissions the mode requires.
5. Run `pa status`, then confirm the first report in the console.

Each machine has its own token. Installation commands and configuration files contain credentials; do not paste them into public discussions or screenshots.

The target machine needs access to `raw.githubusercontent.com` and GitHub downloads; updates need access to GitHub. The scripts read the latest stable release manifest on the target machine and download the matching binary.

| Platform | Architecture | Release file |
| --- | --- | --- |
| Linux | x86_64 | `polaris-agent-<version>-linux-x86_64` |
| Linux | aarch64 | `polaris-agent-<version>-linux-aarch64` |
| macOS | x86_64 | `polaris-agent-<version>-darwin-x86_64` |
| macOS | aarch64 | `polaris-agent-<version>-darwin-aarch64` |
| Windows | x86_64 | `polaris-agent-<version>-windows-x86_64.exe` |

Linux and macOS:

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.sh' | sh -s -- --endpoint 'https://status.example.com' --token 'TOKEN' --mode 'system'
```

Windows PowerShell:

```powershell
& ([scriptblock]::Create((irm 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.ps1'))) -Endpoint 'https://status.example.com' -Token 'TOKEN'
```

Replace the endpoint and token with the values shown in the console. The installer also accepts `--sample-interval`, `--report-interval`, `--install-dir`, `--service-name`, `--include-nics`, `--exclude-nics`, `--include-mounts` and `--exclude-mounts`; `sh install.sh --help` lists the full set.

Adding a machine in the console only registers it. To monitor online status and resource thresholds, create an **Agent** monitor and select that machine. HTTP and TCP monitors can also use the machine as their check source.

### Installation modes

| Mode | Platforms | Behaviour |
| --- | --- | --- |
| `system` | Linux, macOS, Windows | System-wide install; usually requires administrator privileges |
| `user` | Linux, macOS, Windows | Per-user install; stops at logout on macOS |
| `manual` | Linux, macOS, Windows | Prepares the binary and configuration; run in the foreground as printed |
| `docker` | Linux | Generates and starts a Compose deployment; updates replace the container |

Linux uses systemd, OpenRC, supervisord or s6 depending on the host, and may fall back to a startup task or manual mode when no supervisor exists. Linux `user` mode requires `systemd --user`. macOS system mode uses launchd in the system domain; Windows system and user modes use Task Scheduler.

Default program directories are `/usr/local/lib/polaris-agent` for system installs and `~/.local/lib/polaris-agent` for user installs. Windows uses the ProgramData or LocalAppData directory chosen by the installer. When you pass a custom directory or service name, follow the installer output and the `install.json` written next to it.

Docker requires Compose v2. The container only sees the resources the container runtime provides; how much of the host is visible depends on the mounts and permissions you grant.

Reinstalling over a running Agent stops the existing task or service first, waits for the process to exit, replaces the binary and configuration, and then starts it again. On Windows and launchd, a failed start attempts to restore the previous program and service definition.

## Manage the service

```sh
pa status
pa start
pa stop
pa restart
pa logs
pa logs -f
pa logs -n 200
```

| Command | Purpose |
| --- | --- |
| `status` | Service state for this installation |
| `start` / `stop` / `restart` | Calls the actual service manager |
| `logs` | Last 50 lines by default |
| `logs -f` | Follow new output; `Ctrl+C` to stop |
| `logs -n N` | Choose a line count from 1 to 10000 |

Service operations may need elevation, for example `sudo pa restart`. Structured subcommands such as `polaris-agent service status` do the same thing. Manual installs and installations without a supervisor may not support start or stop; the command reports what is actually available.

`pa restart --dry-run` shows the service operation without running it. `run`, `update` and `rollback` do not support `--dry-run`.

Foreground operation, for example during troubleshooting:

```sh
polaris-agent run --config /path/to/config.json
```

Do not start a second instance while a service is running.

The installer registers `pa` and `polaris-agent` for the current scope. If the shell cannot find them, reopen the terminal and check the registration directory printed by the installer.

```sh
polaris-agent command status --config /path/to/config.json
polaris-agent command register --scope user --config /path/to/config.json
polaris-agent command unregister --scope user
```

Registration refuses to overwrite wrappers owned by another program. The wrapper stores only the program and configuration paths, never the machine token. Keep `install.json` with the configuration: management and uninstall both use it.

## Configuration

Precedence is **CLI flags, then `POLARIS_AGENT_*` environment variables, then the JSON file**. When you run through the global wrapper, it already points at the configuration used for this installation; for a foreground run, pass `--config` explicitly.

```json
{
  "endpoint": "https://status.example.com",
  "token": "replace-with-machine-token",
  "sample_interval": 5,
  "report_interval": 60,
  "include_nics": [],
  "exclude_nics": ["lo"],
  "include_mounts": [],
  "exclude_mounts": [],
  "auto_update": false,
  "insecure": false
}
```

| Field | Default or purpose | CLI / environment |
| --- | --- | --- |
| `endpoint` | Site origin or Agent endpoint, required | `--endpoint` / `POLARIS_AGENT_ENDPOINT` |
| `token` | Per-machine credential, required | `--token` / `POLARIS_AGENT_TOKEN` |
| `sample_interval` | 5 seconds | `--sample-interval` / `POLARIS_AGENT_SAMPLE_INTERVAL` |
| `report_interval` | 60 seconds; adjusted after connecting (see below) | `--report-interval` / `POLARIS_AGENT_REPORT_INTERVAL` |
| `include_nics` / `exclude_nics` | Interface filters | Flags / `POLARIS_AGENT_INCLUDE_NICS`, `POLARIS_AGENT_EXCLUDE_NICS` |
| `include_mounts` / `exclude_mounts` | Mount filters | Flags / `POLARIS_AGENT_INCLUDE_MOUNTS`, `POLARIS_AGENT_EXCLUDE_MOUNTS` |
| `auto_update` | Off by default; needs a supervised installation | `--auto-update=true` / `POLARIS_AGENT_AUTO_UPDATE` |
| `collect_gpus` | On by default; detect NVIDIA GPUs | `--collect-gpus=false` / `POLARIS_AGENT_COLLECT_GPUS` |
| `collect_sensors` | On by default; detect Linux hardware temperatures | `--collect-sensors=false` / `POLARIS_AGENT_COLLECT_SENSORS` |
| `collect_containers` | Off by default; opt into read-only Docker/Podman telemetry | `--collect-containers=true` / `POLARIS_AGENT_COLLECT_CONTAINERS` |
| `container_socket` | Optional Unix socket path or HTTP(S) proxy origin; empty means automatic discovery | `--container-socket` / `POLARIS_AGENT_CONTAINER_SOCKET` |
| `insecure` | Off by default; skips certificate checks for reporting and control connections, not for probe targets | `--insecure=true` / `POLARIS_AGENT_INSECURE` |
| `log_file` | Optional log file path | `--log-file` |

Intervals are in seconds and must satisfy `1 <= sample_interval <= report_interval <= 1800`. After connecting, the Agent reads `/api/capabilities`: the Worker backend defaults to one report every 60 seconds, while the Go backend reports once per sample for near real-time updates. An explicit `report_interval` wins; if the capability probe fails, the Agent keeps 60 seconds and keeps running.

Filter values are arrays in JSON and comma-separated on the command line. Unknown fields are rejected, so remove fields from older formats instead of carrying them over.

Installation also manages `service_tier` and `runtime_revision`. Keep these values; do not edit the service tier by hand to get around a capability check.

### GPU and hardware temperatures

Hardware collection is automatic and optional. NVIDIA GPUs on Linux and Windows require a working `nvidia-smi`, available in a standard driver location or an absolute directory in the service's `PATH`. The Agent reports up to 16 GPUs: name, utilization, used/total memory, temperature and power. Unsupported driver metrics remain unknown. GPU utilization, memory and power collection for AMD/Intel, and hardware telemetry on macOS, are not supported yet.

On Linux, the Agent reads up to 64 temperature sensors from `/sys/class/hwmon`, including CPU, disk and GPU temperatures exposed by the kernel. Its service account needs read access to those files. Containers need the appropriate read-only sysfs mounts; NVIDIA collection also needs the driver utility and device access inside the container.

Hardware is discovered at startup and sampled independently every 10 seconds. Missing sources leave no hardware fields or empty cards. NVIDIA queries have a 3-second timeout; failures retry after 60 seconds without delaying ordinary reports. Previously collected readings are cleared on failure or when older than 30 seconds. After adding hardware or installing a driver, restart with `pa restart` to discover it.

To disable either collector, add `"collect_gpus": false` or `"collect_sensors": false` to the installation's JSON configuration and restart. These are local settings; remote configuration preserves them. A server with GPU/temperature telemetry support displays these readings only in the administrator's Agent details; public machine endpoints omit them.

### Docker and Podman containers

Container collection is **off by default**. Enable it in the installation's configuration, then restart:

```json
{
  "collect_containers": true,
  "container_socket": "http://127.0.0.1:2375"
}
```

Add these fields to the existing configuration; keep the endpoint and token. Alternatively, set `POLARIS_AGENT_COLLECT_CONTAINERS=true` and `POLARIS_AGENT_CONTAINER_SOCKET`, or pass `--collect-containers=true --container-socket=http://127.0.0.1:2375`. The HTTP address must be a controlled read-only proxy, such as `docker-socket-proxy`. Allow only GET requests for the container list and stats, with network access restricted to the Agent. Redirects and environment HTTP proxies are not used.

Access to the complete Docker socket grants extensive host privileges, even if mounted read-only. Prefer a restricted proxy or rootless Podman; do not expose the full socket to untrusted environments. The Agent only sends GET requests and provides no container control commands.

For Unix sockets, use an absolute path such as `/var/run/docker.sock` or the expanded `$XDG_RUNTIME_DIR/podman/podman.sock` path. When `container_socket` is empty, Unix systems try `/var/run/docker.sock`, `$XDG_RUNTIME_DIR/docker.sock`, `$XDG_RUNTIME_DIR/podman/podman.sock`, then `/run/podman/podman.sock`. Windows requires an explicit HTTP(S) proxy origin. Container settings are local and preserved by remote updates and reinstalls.

The Agent lists up to 256 containers every 15 seconds, with at most four simultaneous stats requests and a 5-second total budget. Slow or unavailable stats leave CPU/memory unknown while retaining the container's basic state. A failed list retries after 60 seconds; readings older than 45 seconds clear on the next ordinary report. Before any successful reading, no container field is sent. Restart count and uptime are included only when the engine's list supplies those values; container creation time is not used as uptime. Names, images and metrics are visible only in administrator details on a compatible Uptime Polaris server.

### Remote configuration and commands

The server setting `security.allowAgentCommands` is off by default. When enabled, remote configuration requires the Agent to have reported a supported configuration first, and commands additionally require an online machine that advertises the capability.

Seven values can be synchronized: `sample_interval_secs`, `report_interval_secs`, `include_nics`, `exclude_nics`, `include_mounts`, `exclude_mounts` and `auto_update`. The API uses `_secs` names while the local JSON uses `sample_interval` and `report_interval`; do not mix them.

Explicit CLI flags and environment variables still win over remote values. If they conflict, the Agent rejects the whole version, keeps its running configuration, and reports the conflicting fields. Change the service arguments or environment, restart, and then re-check the remote configuration.

Actions are `re_report`, `check_update`, `restart` and `upgrade`. `restart` and `upgrade` only run in supervised installations that report `service_tier: A`; manual installs refuse them because a failed restart would leave the machine unable to report. Arbitrary shell execution is not supported.

A saved remote version is not proof it was applied. Wait for the Agent report that confirms it, and check the command record and receipt.

## Update and uninstall

### Update

```sh
pa update
pa rollback
```

`pa update` downloads a stable release and verifies its integrity. Managed installations can opt into automatic updates in the console; checks run after a successful report, at most once a day, and are off by default. After updating, check `pa status`, the logs, and the reported version in the console.

Update downloads use fixed GitHub HTTPS addresses, regardless of whether `endpoint` uses HTTP or HTTPS. `pa rollback` restores the previous binary kept on the machine; it does not revert server data or configuration. Docker installations update by rebuilding and replacing the container.

### Uninstall

The Agent can remove itself. `pa uninstall` unregisters the terminal command, stops and deletes the service, and removes the binary and configuration:

```sh
pa uninstall
```

On a system installation run it with elevation, for example `sudo pa uninstall`; on Windows use an elevated PowerShell.

Use `pa uninstall --dry-run` to see the plan first, and `--yes` to skip the prompt. Stopping a supervised service terminates the running process, so after confirmation the remaining work is handed to a detached helper that logs to `uninstall.log` next to the binary. Docker installations belong to the Compose project on the host; the command refuses them and prints the `docker compose down` to run there.

The install scripts still work and require the original scope, service name and install directory:

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.sh' -o install.sh
sh install.sh --uninstall
```

```powershell
Invoke-WebRequest 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.ps1' -OutFile install.ps1
& ./install.ps1 -Uninstall
```

Use the original scope, service name and install directory. The uninstaller checks ownership of the service, program, registered commands and configuration before removing them. Removing the host program does not delete console monitors, and deleting a console entry does not uninstall the program.

## Data and connectivity

- Load, connection counts and process counts are separate metrics, not percentages. Windows load is a smoothed processor queue length; Linux and macOS use system load averages, so the numbers are not directly comparable.
- Unreadable metrics display `—`; a measured zero stays `0`.
- Pending snapshots and minute summaries live in memory: up to 4096 samples, 60 snapshots (of which at most 30 may be ahead of server time) and 128 minute buckets. When a buffer is full the Agent logs a warning and may drop data, and an abnormal exit loses unconfirmed data.
- Reports older than 24 hours are acknowledged and ignored. Reports more than 5 minutes ahead are deferred until their timestamps fall inside the accepted window. Repeated minute summaries are deduplicated by time bucket.
- Online timeout is three times the report interval, bounded to 30–3600 seconds. With the default 60-second cadence, a machine is considered offline after about 180 seconds without a report.
- Allow the Agent API and WebSocket through proxies and firewalls; the machine must resolve and reach the site domain.
- Public network identity is discovered from the real interface, bypassing the HTTP proxy, and re-resolved when Fake-IP is detected. On Windows, Clash, Mihomo and Tailscale interfaces are excluded. If discovery fails, the value stays unknown rather than being guessed.
- ICMP needs raw-socket permission. Without it, the Agent reports that the check is unsupported rather than failing silently.

## Troubleshooting

**The console shows the machine as offline.** Run `pa status` and `pa logs -f` on the machine, confirm it can reach the site domain, and confirm the token belongs to this machine entry. Worker backends report every 60 seconds by default, so a short outage can take about three minutes to clear.

**Metrics are missing or wrong.** Check the interface and mount filters first, then what the OS actually exposes. A container sees the container view unless host resources are mounted. A value shown as `—` was not collected; it is not zero.

**`pa` is not found.** Reopen the terminal. If it still is not found, use the wrapper directory printed during installation, or register the command again with `polaris-agent command register`.

**Adding a machine did not create a monitor.** Registering a machine and monitoring it are separate steps. Create an Agent monitor and select the machine; the machine page and the monitor’s public setting are also independent.

**A probe from the Agent fails, though the target works elsewhere.** The probe runs from this machine’s network and with its permissions. Check routing, firewall rules, and for ICMP the raw-socket permission.

More: [Agent guide](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/agents.md) · [Troubleshooting](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/troubleshooting.md)

## License

MIT. See [LICENSE](LICENSE).
