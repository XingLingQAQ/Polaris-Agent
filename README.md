# polaris-agent

[English](#english) · [简体中文](#简体中文)

## 简体中文

`polaris-agent` 为 Uptime Polaris 采集主机指标，也能执行网络探测。`pa` 是它的短命令，两者操作同一套程序、配置和服务。

### 安装

1. 在 Uptime Polaris 控制台的 Agent 页面创建机器。
2. 选择目标平台、安装模式和上报间隔。
3. 在目标机器上执行页面生成的安装命令。
4. 运行 `pa status`，回到控制台确认首次上报。

支持 Linux x86_64、Linux aarch64、Windows x86_64、macOS x86_64 和 macOS aarch64。安装需要能访问 `raw.githubusercontent.com` 和 GitHub 下载服务，更新需要能访问 GitHub。

每台机器使用独立 Token。安装命令和配置里有凭据，不要公开。系统安装通常需要管理员权限。

安装脚本位于本仓库的 [install.sh](install.sh) 和 [install.ps1](install.ps1)，会自动下载并校验最新稳定版。也可以手动下载后执行，将站点地址和 Token 替换为控制台中的值：

Linux / macOS：

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/polaris-agent/main/install.sh' -o install.sh
POLARIS_AGENT_TOKEN='TOKEN' sh install.sh --endpoint 'https://status.example.com' --mode system
```

Windows PowerShell：

```powershell
Invoke-WebRequest 'https://raw.githubusercontent.com/XingLingQAQ/polaris-agent/main/install.ps1' -OutFile install.ps1
& .\install.ps1 -Endpoint 'https://status.example.com' -Token 'TOKEN' -Mode system
```

### 管理服务

```sh
pa status
pa start
pa stop
pa restart
pa logs -f
```

服务操作只在有服务管理器的安装中可用。手动模式按安装输出在前台运行；已有服务时不要再启动第二个实例。

### 配置与更新

连接地址和机器 Token 保存在安装配置里。优先级从高到低：命令行参数、环境变量、JSON 文件。采样和上报间隔必须满足 `1 <= sample_interval <= report_interval <= 1800`，单位秒。连接站点后按后端自动选择默认上报周期：Worker 为 60 秒，Go 为采样间隔；显式配置优先，探测失败时保持 60 秒。

`pa update` 检查并安装更新；`pa rollback` 使用本机保留的上一份二进制。托管安装可以开启自动更新，默认关闭；Docker 通过替换容器更新。

远程配置和操作默认关闭。启用后仍要求机器在线并具备对应能力；保存配置或让指令入队后，要等机器回执。

安装模式、字段、卸载和连接排查见 [Agent 手册](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/agents.md)。

## English

`polaris-agent` collects host metrics and can run network probes for Uptime Polaris. `pa` is its short command; both operate the same program, configuration, and service.

### Install

1. Create a machine on the console’s Agent page.
2. Select the platform, installation mode, and reporting interval.
3. Run the generated command on the target machine.
4. Run `pa status` and confirm the first report in the console.

Supported platforms are Linux x86_64 and aarch64, Windows x86_64, and macOS x86_64 and aarch64. Installation requires access to `raw.githubusercontent.com` and GitHub downloads; updates require GitHub access.

Each machine has its own token. Keep installation commands and configuration private. System installation usually requires administrator privileges.

The installers are [install.sh](install.sh) and [install.ps1](install.ps1) in this repository. They download and verify the latest stable release. To install manually, download the script and pass your site endpoint and machine token:

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/polaris-agent/main/install.sh' -o install.sh
POLARIS_AGENT_TOKEN='TOKEN' sh install.sh --endpoint 'https://status.example.com' --mode system
```

For Windows PowerShell:

```powershell
Invoke-WebRequest 'https://raw.githubusercontent.com/XingLingQAQ/polaris-agent/main/install.ps1' -OutFile install.ps1
& .\install.ps1 -Endpoint 'https://status.example.com' -Token 'TOKEN' -Mode system
```

### Manage

Use `pa status`, `pa start`, `pa stop`, `pa restart` and `pa logs -f`. Available operations depend on the installation mode and service manager. Follow the installer’s output for manual mode; do not run a second instance alongside an existing service.

### Configure and update

The installation configuration stores the endpoint and machine token. Precedence is CLI flags, then environment variables, then JSON. Intervals are in seconds and satisfy `1 <= sample_interval <= report_interval <= 1800`. After connecting, the default cadence follows the backend: 60 seconds on Worker and the sample interval on Go; explicit configuration wins and a failed probe keeps 60 seconds.

`pa update` checks for updates; `pa rollback` uses the previous binary retained on the machine. Managed installations can opt into automatic updates, off by default. Docker updates replace the container.

Remote configuration and commands are disabled by default. They require an online machine with the relevant capabilities. After saving settings or queuing a command, wait for the machine’s acknowledgement.

See the [Agent guide](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/agents.md#agent-guide) for installation modes, configuration, removal, and troubleshooting.

## License

MIT. See [LICENSE](LICENSE).
