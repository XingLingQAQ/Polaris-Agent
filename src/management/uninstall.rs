use super::{Installation, Scope};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub yes: bool,
    pub dry_run: bool,
}

/// 卸载需要处理的文件、目录和服务。
/// Files, directories and service actions this uninstall is responsible for.
struct Plan {
    service: Service,
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
}

enum Service {
    /// A supervisor whose stop action terminates this process.
    Supervisor(&'static str),
    /// Cron stores our entry in the user crontab.
    Cron(String),
    /// Manual installs have no supervisor.
    Manual,
    /// Docker owns the container; the Agent cannot remove it.
    Docker,
}

pub fn execute(config: &Path, options: Options) -> Result<()> {
    let installation = super::load(config)?;
    reject_unfinished_transaction(&installation)?;
    let plan = plan_for(&installation)?;

    if matches!(plan.service, Service::Docker) {
        bail!(
            "Docker installations are removed from the host, not from inside the container.\n\
             Run:\n  docker compose -f {} down\nthen delete {} and its configuration directory.",
            installation
                .extra_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<compose file>".into()),
            installation.executable.display()
        );
    }

    if options.dry_run {
        print_plan(&installation, &plan);
        return Ok(());
    }
    check_privileges(&installation)?;
    confirm(&installation, options.yes)?;

    // The wrapper invokes this binary, so it has to go before the files it
    // points at disappear.
    // 包装器会调用本程序，必须在它指向的文件删除前注销。
    unregister_command(&installation)?;

    // Stopping a supervised service terminates this process, and Windows
    // cannot delete the running binary. Both cases finish in a detached helper.
    // 停止受管理的服务会终止当前进程，Windows 也无法删除运行中的程序，
    // 这两种情况交给脱离的辅助脚本完成。
    if needs_handoff(&installation, &plan.service) {
        return handoff(&installation, &plan);
    }

    apply(&plan)?;
    println!("Agent uninstalled.");
    Ok(())
}

fn needs_handoff(installation: &Installation, service: &Service) -> bool {
    // Only a supervisor stop is guaranteed to kill this process, and Windows
    // cannot delete a running binary. A manual install whose files live outside
    // the running executable can be removed in place.
    // 只有受管理服务一定会终止当前进程，Windows 也无法删除运行中的程序。
    // 手动安装的文件不在运行中的程序里时，可以直接原地删除。
    match service {
        Service::Supervisor(_) => true,
        Service::Manual | Service::Cron(_) => {
            cfg!(windows)
                && std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.canonicalize().ok())
                    == installation.executable.canonicalize().ok()
        }
        Service::Docker => false,
    }
}

/// 系统级安装在删除服务前就会失败，先检查避免删到一半。
/// A system install fails at the service step anyway; check first so nothing is half removed.
fn check_privileges(installation: &Installation) -> Result<()> {
    if installation.scope != Scope::System {
        return Ok(());
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn geteuid() -> u32;
        }
        let euid = unsafe { geteuid() };
        ensure!(
            euid == 0,
            "this is a system installation; re-run as root (sudo pa uninstall)"
        );
    }
    #[cfg(windows)]
    {
        let status = Command::new("net")
            .arg("session")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        ensure!(
            status.map(|status| status.success()).unwrap_or(false),
            "this is a system installation; re-run from an elevated PowerShell"
        );
    }
    Ok(())
}

fn plan_for(installation: &Installation) -> Result<Plan> {
    let service = match installation.manager.as_str() {
        "docker" => Service::Docker,
        "cron" => Service::Cron(cron_marker(&installation.service_name)),
        "manual" => Service::Manual,
        "systemd" => Service::Supervisor("systemd"),
        "launchd" => Service::Supervisor("launchd"),
        "windows-task" => Service::Supervisor("windows-task"),
        "openrc" => Service::Supervisor("openrc"),
        "supervisord" => Service::Supervisor("supervisord"),
        "s6" => Service::Supervisor("s6"),
        other => bail!(
            "this installation records an unknown service manager ({other}); \
             remove it with the installer: sh install.sh --uninstall"
        ),
    };

    let executable = installation.executable.clone();
    let config = installation.config.clone();
    let mut files = vec![
        executable.clone(),
        config.clone(),
        metadata_path(&config),
        marker_path(&config),
        executable.with_extension("previous"),
        executable.with_extension("restart.log"),
        executable.with_extension("update.log"),
    ];
    if let Some(log) = &installation.log_path {
        files.push(log.clone());
    }
    if let Some(extra) = &installation.extra_path {
        // launchd keeps the plist and Docker keeps the compose file here; the
        // service cleanup removes the former, so only list what we delete.
        if matches!(service, Service::Docker) {
            files.push(extra.clone());
        }
    }
    files.retain(|path| path != Path::new("") && path.exists());
    files.dedup();

    // rmdir-style cleanup: a directory holding anything else survives.
    // 目录非空就保留，避免误删共享的安装根目录。
    let mut directories = vec![
        executable.parent().unwrap_or(Path::new(".")).to_path_buf(),
        config.parent().unwrap_or(Path::new(".")).to_path_buf(),
    ];
    directories.dedup();

    Ok(Plan {
        service,
        files,
        directories,
    })
}

fn cron_marker(service_name: &str) -> String {
    format!("# polaris-agent-managed:{service_name}")
}

fn metadata_path(config: &Path) -> PathBuf {
    config
        .parent()
        .unwrap_or(Path::new("."))
        .join("install.json")
}

fn marker_path(config: &Path) -> PathBuf {
    config
        .parent()
        .unwrap_or(Path::new("."))
        .join(".polaris-agent-managed")
}

fn reject_unfinished_transaction(installation: &Installation) -> Result<()> {
    let transaction = installation
        .config
        .parent()
        .unwrap_or(Path::new("."))
        .join(".polaris-agent-transaction");
    ensure!(
        !transaction.exists(),
        "an unfinished installation transaction is present at {}; rerun the installer without --uninstall to recover it first",
        transaction.display()
    );
    Ok(())
}

fn print_plan(installation: &Installation, plan: &Plan) {
    println!("would uninstall {}", installation.executable.display());
    match &plan.service {
        Service::Supervisor(manager) => {
            println!("  service: {manager} ({})", installation.service_name)
        }
        Service::Cron(marker) => println!("  crontab entry: {marker}"),
        Service::Manual => println!("  service: none (manual installation)"),
        Service::Docker => println!("  service: docker compose project"),
    }
    for path in &plan.files {
        println!("  remove: {}", path.display());
    }
}

fn confirm(installation: &Installation, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    print!(
        "Remove the Agent at {} and its service and commands? [y/N] ",
        installation.executable.display()
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    ensure!(
        matches!(answer.trim(), "y" | "Y" | "yes" | "YES"),
        "cancelled"
    );
    let _ = installation.scope == Scope::System;
    Ok(())
}

fn unregister_command(installation: &Installation) -> Result<()> {
    if installation.registration.is_none() {
        return Ok(());
    }
    let status = Command::new(&installation.executable)
        .args([
            "command",
            "unregister",
            "--config",
            &installation.config.to_string_lossy(),
        ])
        .status()
        .context("cannot run command unregister")?;
    ensure!(
        status.success(),
        "cannot remove the registered terminal command; run `pa command unregister` first"
    );
    Ok(())
}

/// 立即执行剩余删除；调用方保证不会因此被服务管理器杀掉。
/// Apply the remaining removals in this process; the caller guarantees the
/// service manager will not kill it.
fn apply(plan: &Plan) -> Result<()> {
    if let Service::Cron(marker) = &plan.service {
        remove_cron(marker)?;
    }
    for path in &plan.files {
        remove_file(path)?;
    }
    for path in &plan.directories {
        let _ = fs::remove_dir(path);
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

fn remove_cron(marker: &str) -> Result<()> {
    let listing = Command::new("crontab")
        .arg("-l")
        .output()
        .context("cannot read crontab")?;
    let text = String::from_utf8_lossy(&listing.stdout);
    let kept: Vec<&str> = text.lines().filter(|line| !line.contains(marker)).collect();
    if kept.len() == text.lines().count() {
        return Ok(());
    }
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("cannot rewrite crontab")?;
    {
        let mut stdin = child.stdin.take().context("cannot open crontab stdin")?;
        for line in kept {
            writeln!(stdin, "{line}")?;
        }
    }
    let status = child.wait()?;
    ensure!(status.success(), "cannot rewrite the crontab");
    Ok(())
}

#[cfg(not(windows))]
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 生成脱离当前进程的清理脚本，等本进程退出后完成删除。
/// Build a detached cleanup script that finishes after this process exits.
fn handoff(installation: &Installation, plan: &Plan) -> Result<()> {
    let directory = installation
        .config
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    #[cfg(windows)]
    let extension = "ps1";
    #[cfg(not(windows))]
    let extension = "sh";
    let script = directory.join(format!(".polaris-uninstall-{}.{extension}", std::process::id()));
    let ready = directory.join(format!(".polaris-uninstall-{}.ready", std::process::id()));

    ensure!(
        !script.exists() && !ready.exists(),
        "an uninstall handoff is already present at {}",
        script.display()
    );

    #[cfg(windows)]
    let body = windows_script(installation, plan, &ready)?;
    #[cfg(not(windows))]
    let body = posix_script(installation, plan, &ready)?;

    let outcome = (|| -> Result<()> {
        crate::storage::write_private(&script, body.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        }
        // The record has to come from here: a non-interactive PowerShell
        // cannot reliably start a transcript, and the POSIX helper's own
        // redirect would point into the directory being deleted.
        // 运行记录由父进程写：非交互 PowerShell 无法可靠启动转录，
        // POSIX 辅助脚本的自身重定向又会指向正在删除的目录。
        if let Some(mut log) = open_helper_log(&installation) {
            use std::io::Write;
            let _ = writeln!(
                log,
                "uninstall helper started: {} ({}), service {}",
                script.display(),
                installation.manager,
                installation.service_name
            );
        }
        let mut child = spawn_helper(&script, &ready)?;
        let started = std::time::Instant::now();
        loop {
            match fs::read_to_string(&ready) {
                Ok(state) if state.trim() == "ready" => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("cannot read the uninstall handoff state"),
            }
            if let Some(status) = child.try_wait()? {
                bail!("the uninstall helper stopped before taking over ({status})");
            }
            if started.elapsed() > std::time::Duration::from_secs(15) {
                let _ = child.kill();
                bail!("the uninstall helper did not start in time");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(())
    })();

    if outcome.is_err() {
        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&ready);
    }
    outcome?;

    eprintln!(
        "Uninstall continues after this process exits.\n  log: {}",
        helper_log(installation).display()
    );
    Ok(())
}

fn helper_log(installation: &Installation) -> PathBuf {
    // Keep the record outside the installation directories: the helper removes
    // them, and the log is only useful afterwards.
    // 日志放在安装目录外，因为辅助脚本会删除那些目录，而日志正是事后排查用的。
    std::env::temp_dir().join(format!(
        "polaris-uninstall-{}.log",
        installation.service_name
    ))
}

#[cfg(not(windows))]
fn spawn_helper(script: &Path, _ready: &Path) -> Result<std::process::Child> {
    // A new session survives the service manager stopping this process.
    // 新会话不会被服务管理器对当前进程的停止影响。
    Command::new("sh")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot start the uninstall helper")
}

#[cfg(windows)]
fn spawn_helper(script: &Path, _ready: &Path) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0100_0000 | 0x0800_0000 | 0x0000_0200)
        .spawn()
        .context("cannot start the Windows uninstall helper")
}

/// 辅助脚本的运行记录：由父进程写入，脚本本身不依赖 PowerShell 转录。
/// Helper record: written by the parent, so the script needs no PowerShell transcript.
fn open_helper_log(installation: &Installation) -> Option<std::fs::File> {
    let path = helper_log(installation);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

#[cfg(not(windows))]
fn posix_script(installation: &Installation, plan: &Plan, ready: &Path) -> Result<String> {
    // Collect first: mixing `&PathBuf` and `&Path` items in one chain does not
    // type-check.
    // 先收集：同一条链里混用 `&PathBuf` 和 `&Path` 无法通过类型检查。
    let checked: Vec<&Path> = plan
        .files
        .iter()
        .map(PathBuf::as_path)
        .chain(plan.directories.iter().map(PathBuf::as_path))
        .chain(std::iter::once(installation.config.as_path()))
        .chain(std::iter::once(ready))
        .collect();
    for path in checked {
        ensure!(
            !path.to_string_lossy().contains(['\n', '\r', '\'']),
            "uninstall path contains a character the helper cannot handle: {}",
            path.display()
        );
    }
    let mut out = String::from("#!/bin/sh\nset -u\n");
    out.push_str(&format!(
        "exec >>{} 2>&1\n",
        quote(&helper_log(installation).to_string_lossy())
    ));
    out.push_str(&format!(
        "printf ready > {}\n",
        quote(&ready.to_string_lossy())
    ));
    // Re-exec'd through systemd-run keeps the handshake above but must not
    // wait on the original process again.
    // 经 systemd-run 重新执行后会跳过等待，握手已经完成。
    out.push_str(&format!(
        "if [ -z \"${{POLARIS_UNINSTALL_DETACHED:-}}\" ]; then while kill -0 {} 2>/dev/null; do sleep 0.2; done; fi\n",
        std::process::id()
    ));

    let name = quote(&installation.service_name);
    match &plan.service {
        Service::Supervisor(manager) => match *manager {
            "systemd" => {
                let user = if installation.scope == Scope::User {
                    " --user"
                } else {
                    ""
                };
                // Leave the service cgroup first: KillMode=control-group would
                // otherwise take this helper down with the Agent.
                // 先脱离服务的 cgroup，否则 KillMode=control-group 会把辅助脚本一起杀掉。
                out.push_str(&format!(
                    "if command -v systemd-run >/dev/null 2>&1 && [ -z \"${{POLARIS_UNINSTALL_DETACHED:-}}\" ]; then\n  POLARIS_UNINSTALL_DETACHED=1 exec systemd-run --quiet --collect --unit=polaris-uninstall-$$ --scope sh \"$0\"\nfi\nsystemctl{user} disable --now {name} >/dev/null 2>&1 || true\n"
                ));
                let unit = if installation.scope == Scope::User {
                    user_unit_path(&installation.service_name)?
                } else {
                    PathBuf::from(format!(
                        "/etc/systemd/system/{}.service",
                        installation.service_name
                    ))
                };
                out.push_str(&format!(
                    "rm -f {}\nsystemctl{user} daemon-reload >/dev/null 2>&1 || true\n",
                    quote(&unit.to_string_lossy())
                ));
            }
            "launchd" => {
                let plist = installation
                    .extra_path
                    .as_ref()
                    .context("launchd plist path is missing from installation information")?;
                let domain = if installation.scope == Scope::System {
                    "system".to_string()
                } else {
                    format!(
                        "gui/{}",
                        installation
                            .user_id
                            .as_deref()
                            .context("launchd user ID is missing")?
                    )
                };
                out.push_str(&format!(
                    "launchctl bootout {} >/dev/null 2>&1 || true\nrm -f {}\n",
                    quote(&format!("{domain}/{}", installation.service_name)),
                    quote(&plist.to_string_lossy())
                ));
            }
            "openrc" => {
                out.push_str(&format!(
                    "rc-service {name} stop >/dev/null 2>&1 || true\nrc-update del {name} >/dev/null 2>&1 || true\nrm -f {}\n",
                    quote(&format!("/etc/init.d/{}", installation.service_name))
                ));
            }
            "supervisord" => {
                out.push_str(&format!(
                    "supervisorctl stop {name} >/dev/null 2>&1 || true\nrm -f {}\nsupervisorctl update >/dev/null 2>&1 || true\n",
                    quote(&format!(
                        "/etc/supervisor/conf.d/{}.conf",
                        installation.service_name
                    ))
                ));
            }
            "s6" => {
                let directory = format!("/etc/services.d/{}", installation.service_name);
                out.push_str(&format!(
                    "s6-svc -d {} >/dev/null 2>&1 || true\nrm -f {}\nrmdir {} 2>/dev/null || true\n",
                    quote(&format!("/run/service/{}", installation.service_name)),
                    quote(&format!("{directory}/run")),
                    quote(&directory)
                ));
            }
            other => bail!("unsupported service manager in the uninstall helper: {other}"),
        },
        // Guard the read: a failing `crontab -l` must not be piped into
        // `crontab -`, which would replace the whole table with nothing.
        // 先确认读取成功，否则失败的空输出写回去会清空整个 crontab。
        Service::Cron(marker) => out.push_str(&format!(
            "tmp=$(mktemp) || tmp=\nif [ -n \"$tmp\" ] && crontab -l >\"$tmp\" 2>/dev/null; then\n  grep -v -F {} \"$tmp\" | crontab - 2>/dev/null || true\nfi\n[ -z \"$tmp\" ] || rm -f \"$tmp\"\n",
            quote(marker)
        )),
        Service::Manual => {}
        Service::Docker => unreachable!("Docker is rejected before the handoff"),
    }

    for path in &plan.files {
        out.push_str(&format!("rm -f {}\n", quote(&path.to_string_lossy())));
    }
    for path in &plan.directories {
        out.push_str(&format!("rmdir {} 2>/dev/null || true\n", quote(&path.to_string_lossy())));
    }
    out.push_str(&format!(
        "rm -f {}\nrm -f \"$0\"\nexit 0\n",
        quote(&ready.to_string_lossy())
    ));
    Ok(out)
}

#[cfg(not(windows))]
fn user_unit_path(name: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is required")?).join(".config"),
    };
    Ok(base.join("systemd/user").join(format!("{name}.service")))
}

#[cfg(windows)]
fn windows_script(installation: &Installation, plan: &Plan, ready: &Path) -> Result<String> {
    let mut removals = String::new();
    for path in &plan.files {
        let literal = path.to_string_lossy().replace('\'', "''");
        removals.push_str(&format!(
            "  Remove-Item -LiteralPath '{literal}' -Force -ErrorAction SilentlyContinue\n"
        ));
    }
    for path in &plan.directories {
        let literal = path.to_string_lossy().replace('\'', "''");
        removals.push_str(&format!(
            "  Remove-Item -LiteralPath '{literal}' -Force -ErrorAction SilentlyContinue\n"
        ));
    }
    let task = installation.service_name.replace('\'', "''");
    let binary = installation.executable.to_string_lossy().replace('\'', "''");
    let ready = ready.to_string_lossy().replace('\'', "''");
    let body = format!(
        r#"$ErrorActionPreference = 'Continue'
$binary = [IO.Path]::GetFullPath('{binary}')
$task = '{task}'
try {{
  $parent = Get-Process -Id {pid} -ErrorAction Stop
  if ([IO.Path]::GetFullPath($parent.Path) -ne $binary) {{ throw 'parent does not match the installed Agent' }}
}} catch {{
  Write-Error $_ -ErrorAction Continue
}}
[IO.File]::WriteAllText('{ready}', 'ready')
if ($parent) {{ $parent.WaitForExit(120000) | Out-Null }}
$deadline = [DateTime]::UtcNow.AddSeconds(30)
while ((Get-ScheduledTask -TaskPath '\' -TaskName $task -ErrorAction SilentlyContinue).State -eq 'Running') {{
  if ([DateTime]::UtcNow -ge $deadline) {{ break }}
  Start-Sleep -Milliseconds 250
}}
Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
{removals}Remove-Item -LiteralPath '{ready}' -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
"#,
        pid = std::process::id(),
    );
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 写出可被 `super::load` 接受的安装记录。
    /// Write an installation record that `super::load` accepts.
    fn installation(root: &Path, manager: &str) -> Installation {
        let executable = root.join(if cfg!(windows) {
            "polaris-agent.exe"
        } else {
            "polaris-agent"
        });
        let config = root.join("config.json");
        fs::write(&executable, b"binary").unwrap();
        fs::write(&config, b"{}").unwrap();
        let record = Installation {
            schema: 1,
            executable,
            config: normalize(&config),
            manager: manager.into(),
            service_name: "polaris-test".into(),
            scope: Scope::User,
            mode: "manual".into(),
            install_dir: None,
            log_path: None,
            extra_path: None,
            user_id: None,
            registration: None,
        };
        fs::write(
            root.join("install.json"),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        record
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "polaris-uninstall-{}-{name}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        normalize(&root)
    }

    /// 与 `management::absolute` 一致：去掉 Windows 的 `\\?\` 前缀。
    /// Match `management::absolute`, which strips the Windows `\\?\` prefix.
    fn normalize(path: &Path) -> std::path::PathBuf {
        let resolved = path.canonicalize().unwrap();
        #[cfg(windows)]
        {
            let text = resolved.to_string_lossy();
            if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
                return std::path::PathBuf::from(format!("\\\\{rest}"));
            }
            if let Some(rest) = text.strip_prefix("\\\\?\\") {
                return std::path::PathBuf::from(rest);
            }
        }
        resolved
    }

    #[test]
    fn plan_covers_installation_files() {
        let root = temp_root("plan");
        let plan = plan_for(&installation(&root, "manual")).unwrap();
        assert!(matches!(plan.service, Service::Manual));
        let executable = root.join(if cfg!(windows) {
            "polaris-agent.exe"
        } else {
            "polaris-agent"
        });
        assert!(plan.files.contains(&executable));
        assert!(plan.files.contains(&root.join("config.json")));
        assert!(plan.files.contains(&root.join("install.json")));
        // A missing optional file must not appear in the removal list.
        assert!(!plan.files.iter().any(|path| path.ends_with(".previous")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_never_lists_missing_paths() {
        let root = temp_root("missing");
        let plan = plan_for(&installation(&root, "systemd")).unwrap();
        assert!(plan.files.iter().all(|path| path.exists()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn docker_is_rejected() {
        let root = temp_root("docker");
        let record = installation(&root, "docker");
        let error = execute(&record.config, Options { yes: true, dry_run: false }).unwrap_err();
        assert!(error.to_string().contains("docker compose"), "{error}");
        assert!(record.config.exists(), "nothing should be removed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unfinished_transaction_blocks_uninstall() {
        let root = temp_root("transaction");
        let record = installation(&root, "manual");
        fs::create_dir_all(root.join(".polaris-agent-transaction")).unwrap();
        let error = execute(&record.config, Options { yes: true, dry_run: false }).unwrap_err();
        assert!(error.to_string().contains("unfinished installation transaction"));
        assert!(record.config.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_uninstall_removes_files_and_keeps_other_directories() {
        let root = temp_root("manual");
        let record = installation(&root, "manual");
        // A neighbouring file under the same config directory must survive the
        // rmdir-style cleanup.
        let kept = root.join("keep.txt");
        fs::write(&kept, b"keep").unwrap();
        execute(&record.config, Options { yes: true, dry_run: false }).unwrap();
        assert!(!record.config.exists());
        assert!(!root.join("install.json").exists());
        assert!(kept.exists(), "unrelated files must survive");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dry_run_changes_nothing() {
        let root = temp_root("dry-run");
        let record = installation(&root, "manual");
        execute(
            &record.config,
            Options {
                yes: true,
                dry_run: true,
            },
        )
        .unwrap();
        assert!(record.config.exists());
        assert!(root.join("install.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    // The POSIX helper is only compiled on Unix; these assertions are guarded
    // here so Windows builds still compile the module.
    // POSIX 辅助脚本只在 Unix 编译，这里加条件，保证 Windows 构建也能编译。
    #[cfg(not(windows))]
    #[test]
    fn posix_helper_guards_crontab_and_removes_files() {
        let root = temp_root("helper");
        let record = installation(&root, "cron");
        let plan = plan_for(&record).unwrap();
        let ready = root.join(".ready");
        let script = posix_script(&record, &plan, &ready).unwrap();
        // A failed `crontab -l` must not be piped into `crontab -`.
        assert!(script.contains("&& crontab -l"), "{script}");
        assert!(script.contains("systemd-run") == false, "{script}");
        for path in &plan.files {
            assert!(script.contains(&format!("rm -f '{}'", path.display())), "{path:?}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn systemd_helper_leaves_the_service_cgroup_first() {
        let root = temp_root("systemd-helper");
        let record = installation(&root, "systemd");
        let plan = plan_for(&record).unwrap();
        let script = posix_script(&record, &plan, &root.join(".ready")).unwrap();
        let detach = script.find("systemd-run").unwrap();
        let stop = script.find("systemctl disable").unwrap();
        assert!(detach < stop, "the helper must detach before stopping: {script}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_waits_for_the_process_and_removes_the_task() {
        let root = temp_root("windows-helper");
        let record = installation(&root, "windows-task");
        let plan = plan_for(&record).unwrap();
        let ready = root.join(".ready");
        let script = windows_script(&record, &plan, &ready).unwrap();
        // The scheduled task cannot be deleted while it is still running.
        let wait = script.find("WaitForExit").unwrap();
        let delete = script.find("Unregister-ScheduledTask").unwrap();
        assert!(wait < delete, "{script}");
        assert!(script.contains("'ready'"), "{script}");
        fs::remove_dir_all(root).unwrap();
    }
}
