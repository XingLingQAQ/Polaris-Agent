use crate::{
    config::{CliArgs, Config},
    management::{self, CommandAction, RecordArgs, ServiceAction, ServiceArgs},
};
use anyhow::Result;
use clap::Subcommand;
use std::{io::IsTerminal, path::PathBuf};

/// 配置日志输出；指定文件不存在时先以私有权限创建。
/// Configure logging; create a missing log file with private permissions.
pub fn initialize_logging(log_file: Option<PathBuf>) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
        .from_env_lossy();
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::stdout().is_terminal());
    if let Some(path) = log_file {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            crate::storage::write_private(&path, b"")?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        subscriber
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(options.open(path)?))
            .init();
    } else {
        subscriber.init();
    }
    Ok(())
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    /// Run the monitoring agent in the foreground.
    /// 在前台运行监控 Agent。
    Run,
    /// Manage this installation's service and logs.
    /// 管理此安装的服务和日志。
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    #[command(flatten)]
    ServiceShortcut(ServiceAction),
    /// Register or remove the global terminal command.
    /// 注册或移除全局终端命令。
    Command {
        #[command(subcommand)]
        action: CommandAction,
    },
    /// Download and apply an Agent update.
    /// 下载并应用 Agent 更新。
    Update,
    /// Restore the previously installed binary.
    /// 恢复先前安装的可执行文件。
    Rollback,
    /// Remove this installation, its service and its registered commands.
    /// 卸载本次安装，包括服务、已注册命令和程序文件。
    Uninstall {
        /// Skip the confirmation prompt.
        /// 跳过确认提示。
        #[arg(long)]
        yes: bool,
    },
    #[command(hide = true)]
    RecordInstallation(RecordArgs),
}

/// 执行管理子命令；前台运行时返回监控循环所需配置。
/// Dispatch management commands; return monitoring configuration for foreground runs.
pub fn dispatch(mut args: CliArgs) -> Result<Option<Config>> {
    anyhow::ensure!(
        !args.dry_run
            || matches!(
                args.command.as_ref(),
                Some(
                    AgentCommand::Service { .. }
                        | AgentCommand::ServiceShortcut(_)
                        | AgentCommand::Command { .. }
                        | AgentCommand::Uninstall { .. }
                )
            ),
        "--dry-run is supported only for service management, command registration and uninstall"
    );
    let path = Config::resolve_config_path(args.config.as_deref());
    match args.command.take() {
        Some(AgentCommand::Service { action } | AgentCommand::ServiceShortcut(action)) => {
            management::manage_service(
                &path,
                ServiceArgs {
                    action,
                    dry_run: args.dry_run,
                },
            )?;
            Ok(None)
        }
        Some(AgentCommand::Command { action }) => {
            management::manage_command(&path, action)?;
            Ok(None)
        }
        Some(AgentCommand::Uninstall { yes }) => {
            management::uninstall::execute(
                &path,
                management::uninstall::Options {
                    yes,
                    dry_run: args.dry_run,
                },
            )?;
            Ok(None)
        }
        Some(AgentCommand::RecordInstallation(record)) => {
            management::record(&path, record)?;
            Ok(None)
        }
        None => anyhow::bail!("a command is required; run polaris-agent --help"),
        action => {
            if matches!(action, Some(AgentCommand::Update | AgentCommand::Rollback)) {
                if let Ok(installation) = management::load(&path) {
                    anyhow::ensure!(installation.manager != "docker", "Docker installations are updated by rebuilding and replacing the container image");
                }
                let rollback = matches!(action, Some(AgentCommand::Rollback));
                crate::updater::update(rollback)?;
                return Ok(None);
            }
            Ok(Some(Config::from_args(args)?))
        }
    }
}
