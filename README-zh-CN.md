# polaris-agent

[English](README.md) · [简体中文](README-zh-CN.md)

`polaris-agent` 是 [Uptime Polaris](https://github.com/XingLingQAQ/UptimePolaris) 的主机探针，采集 CPU、内存、磁盘、网络和负载，也可以从所在机器执行网络探测。`pa` 是短命令，两者操作同一套程序、配置和服务。

## 安装

1. 在 Uptime Polaris 中，于 **设置 → 展示设置** 开启机器展示页，然后进入 Agent 管理页面创建机器。
2. 选择平台、安装模式和采样、上报间隔。
3. 在正式域名下复制生成的安装命令。
4. 在目标机器上，用该模式需要的权限执行。
5. 运行 `pa status`，回到控制台确认首次上报。

每台机器使用独立 Token。安装命令和配置文件包含凭据，不要发到公共讨论或截图里。

目标机器需要能访问 `raw.githubusercontent.com` 和 GitHub 下载服务，更新需要能访问 GitHub。安装脚本在目标机器上读取最新稳定版的清单，再下载对应版本。

| 平台 | 架构 | 发布文件 |
| --- | --- | --- |
| Linux | x86_64 | `polaris-agent-<版本>-linux-x86_64` |
| Linux | aarch64 | `polaris-agent-<版本>-linux-aarch64` |
| macOS | x86_64 | `polaris-agent-<版本>-darwin-x86_64` |
| macOS | aarch64 | `polaris-agent-<版本>-darwin-aarch64` |
| Windows | x86_64 | `polaris-agent-<版本>-windows-x86_64.exe` |

Linux / macOS：

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.sh' | sh -s -- --endpoint 'https://status.example.com' --token 'TOKEN' --mode 'system'
```

Windows PowerShell：

```powershell
& ([scriptblock]::Create((irm 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.ps1'))) -Endpoint 'https://status.example.com' -Token 'TOKEN'
```

把站点地址和 Token 换成控制台给出的实际值。安装脚本还支持 `--sample-interval`、`--report-interval`、`--install-dir`、`--service-name`、`--include-nics`、`--exclude-nics`、`--include-mounts` 和 `--exclude-mounts`，完整列表见 `sh install.sh --help`。

在控制台添加机器只是登记。要监控在线状态和资源告警，需要新增 **Agent** 类型的监控并选择这台机器；HTTP 和 TCP 监控也可以把它设为检查来源。

### 安装模式

| 模式 | 平台 | 行为 |
| --- | --- | --- |
| `system` | Linux、macOS、Windows | 系统级安装，通常需要管理员权限 |
| `user` | Linux、macOS、Windows | 当前用户安装；macOS 在注销后停止 |
| `manual` | Linux、macOS、Windows | 准备程序和配置，按安装输出在前台运行 |
| `docker` | Linux | 生成并启动 Compose 部署，更新时替换容器 |

Linux 会按机器环境使用 systemd、OpenRC、supervisord 或 s6，没有守护进程时可能退到启动任务或手动模式。Linux 的 `user` 模式要求 `systemd --user`。macOS 系统模式使用 system 域的 launchd，Windows 的系统模式和用户模式都使用计划任务。

系统安装的默认程序目录是 `/usr/local/lib/polaris-agent`，用户安装是 `~/.local/lib/polaris-agent`。Windows 使用安装器确定的 ProgramData 或 LocalAppData 目录。自定义了安装目录或服务名时，以安装输出和旁边的 `install.json` 为准。

Docker 需要 Compose v2。容器里的进程只能看到容器提供的系统视图，能看到多少宿主机资源取决于实际挂载和权限。

重复安装会先停止已有的任务或服务，等待进程退出，再替换程序和配置并重新启动。Windows 和 launchd 在启动失败时会尝试恢复原程序和原来的服务配置。

## 管理服务

```sh
pa status
pa start
pa stop
pa restart
pa logs
pa logs -f
pa logs -n 200
```

| 命令 | 作用 |
| --- | --- |
| `status` | 查询当前安装对应的服务状态 |
| `start` / `stop` / `restart` | 调用实际的服务管理器 |
| `logs` | 读取日志，默认最后 50 行 |
| `logs -f` | 持续查看新日志，Ctrl+C 结束 |
| `logs -n N` | 指定行数，1–10000 |

系统安装的操作可能需要管理员权限，例如 `sudo pa restart`。结构化子命令同样可用，例如 `polaris-agent service status`。手动安装或没有服务管理器的安装不一定支持启停，命令会报告实际能力。

`pa restart --dry-run` 只显示将要调用的服务操作，不实际执行。`run`、`update` 和 `rollback` 不支持 `--dry-run`。

前台运行，例如排查问题时：

```sh
polaris-agent run --config /path/to/config.json
```

已有服务在运行时，不要再启动第二个实例。

安装器会为当前范围注册 `pa` 和 `polaris-agent`。终端识别不到命令时，先重新打开终端，再看安装输出里的注册目录。

```sh
polaris-agent command status --config /path/to/config.json
polaris-agent command register --scope user --config /path/to/config.json
polaris-agent command unregister --scope user
```

注册器会检查文件归属，遇到其他程序占用同名命令时拒绝覆盖。包装器只保存程序和配置路径，不包含机器 Token。`install.json` 和配置放在一起，管理和卸载都要用到，不要删除。

## 配置

优先级从高到低：**命令行参数、`POLARIS_AGENT_*` 环境变量、JSON 配置文件**。通过全局包装器运行时，它已经绑定这次安装的配置位置；自己前台运行时，建议显式加 `--config`。

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

| 字段 | 默认值或用途 | CLI / 环境变量 |
| --- | --- | --- |
| `endpoint` | 站点 origin 或 Agent 端点，必需 | `--endpoint` / `POLARIS_AGENT_ENDPOINT` |
| `token` | 每台机器的凭据，必需 | `--token` / `POLARIS_AGENT_TOKEN` |
| `sample_interval` | 默认 5 秒 | `--sample-interval` / `POLARIS_AGENT_SAMPLE_INTERVAL` |
| `report_interval` | 默认 60 秒；连接后按后端调整（见下） | `--report-interval` / `POLARIS_AGENT_REPORT_INTERVAL` |
| `include_nics` / `exclude_nics` | 网卡过滤列表 | 对应参数 / `POLARIS_AGENT_INCLUDE_NICS`、`POLARIS_AGENT_EXCLUDE_NICS` |
| `include_mounts` / `exclude_mounts` | 挂载点过滤列表 | 对应参数 / `POLARIS_AGENT_INCLUDE_MOUNTS`、`POLARIS_AGENT_EXCLUDE_MOUNTS` |
| `auto_update` | 默认关闭；需要具备自动重启能力的托管安装 | `--auto-update=true` / `POLARIS_AGENT_AUTO_UPDATE` |
| `collect_gpus` | 默认开启，自动检测 NVIDIA GPU | `--collect-gpus=false` / `POLARIS_AGENT_COLLECT_GPUS` |
| `collect_sensors` | 默认开启，自动检测 Linux 硬件温度 | `--collect-sensors=false` / `POLARIS_AGENT_COLLECT_SENSORS` |
| `collect_containers` | 默认关闭，显式开启 Docker／Podman 只读遥测 | `--collect-containers=true` / `POLARIS_AGENT_COLLECT_CONTAINERS` |
| `container_socket` | 可选 Unix Socket 路径或 HTTP(S) 代理 origin；留空自动检测 | `--container-socket` / `POLARIS_AGENT_CONTAINER_SOCKET` |
| `insecure` | 默认关闭；跳过上报和控制连接的证书校验，不影响独立的探测目标客户端 | `--insecure=true` / `POLARIS_AGENT_INSECURE` |
| `log_file` | 可选日志文件路径 | `--log-file` |

采样和上报间隔的单位是秒，必须满足 `1 <= sample_interval <= report_interval <= 1800`。Agent 连接站点后会读取 `/api/capabilities`：Worker 后端默认每 60 秒上报一次，Go 后端按采样间隔逐个上报，接近实时。显式配置了 `report_interval` 时以配置为准；探测失败时保持 60 秒并继续运行。

JSON 中的过滤值用数组，命令行用逗号分隔。未知字段会被拒绝，请删除旧格式的字段，不要继续保留。

安装过程还会管理 `service_tier` 和 `runtime_revision`。这些值要保留，不要靠手改服务等级绕过能力检查。

### GPU 与硬件温度

硬件采集默认自动检测，可以单独关闭。Linux 和 Windows 上的 NVIDIA GPU 需要可用的 `nvidia-smi`，程序应位于驱动标准目录或服务 `PATH` 中的绝对路径目录。Agent 最多上报 16 张卡的名称、利用率、显存用量、温度和功耗，驱动不支持的指标保留为未知。目前不支持 AMD、Intel GPU 的利用率、显存和功耗采集，也不支持 macOS 硬件遥测。

Linux 从 `/sys/class/hwmon` 读取最多 64 个温度传感器，包括内核暴露的 CPU、磁盘和 GPU 温度。服务账户需要这些文件的读取权限。容器需要挂载相应的只读 sysfs 路径；采集 NVIDIA GPU 还需要容器内可用的驱动工具和设备访问权限。

硬件在启动时检测，每 10 秒独立采集一次。没有可用来源时，不附加硬件字段，也不显示空卡片。NVIDIA 查询限时 3 秒，失败后等待 60 秒重试，不影响常规上报。已采集的读数在失败或超过 30 秒未更新时清除。新增硬件或安装驱动后，执行 `pa restart` 重新检测。

需要关闭时，在安装使用的 JSON 配置中加入 `"collect_gpus": false` 或 `"collect_sensors": false`，然后重启。这两项只由本地配置控制，远程配置更新会保留原值。服务端需要支持 GPU 与温度遥测；数据只显示在管理员的 Agent 详情中，公开机器接口不返回这些字段。

### Docker 与 Podman 容器

容器采集**默认关闭**。在现有安装配置中增加下面两项，保留原有站点地址和 Token，然后重启：

```json
{
  "collect_containers": true,
  "container_socket": "http://127.0.0.1:2375"
}
```

也可设置环境变量 `POLARIS_AGENT_COLLECT_CONTAINERS=true`、`POLARIS_AGENT_CONTAINER_SOCKET`，或使用 `--collect-containers=true --container-socket=http://127.0.0.1:2375`。HTTP 地址应指向受控的只读代理，例如 `docker-socket-proxy`。仅允许容器列表和 stats 的 GET 请求，并限制为 Agent 可访问。采集不会跟随重定向，也不会使用环境 HTTP 代理。

访问完整 Docker Socket 等价于获得很高的主机权限，即使只读挂载也不能限制引擎操作。优先使用受限代理或 rootless Podman，不要向不受控环境暴露完整 Socket。Agent 只发送 GET 请求，不提供容器启动、停止或执行命令的能力。

Unix Socket 使用绝对路径，例如 `/var/run/docker.sock` 或展开后的 `$XDG_RUNTIME_DIR/podman/podman.sock`。留空时，Unix 系统依次尝试 `/var/run/docker.sock`、`$XDG_RUNTIME_DIR/docker.sock`、`$XDG_RUNTIME_DIR/podman/podman.sock`、`/run/podman/podman.sock`。Windows 需显式配置 HTTP(S) 代理 origin。远程配置更新和重装升级会保留这两项本地设置。

每 15 秒采集最多 256 个容器，stats 最多并发 4 个请求，整轮预算为 5 秒。stats 超时或失败时仍保留基础状态，CPU、内存保持未知。列表失败后等待 60 秒重试；读数超过 45 秒未更新时，在下一次常规上报中清除。从未采集成功时不发送容器字段。重启次数和运行时长仅在引擎列表提供对应值时上报，不把容器创建时间当作运行时长。兼容的 Uptime Polaris 服务端仅在管理员详情中展示容器信息。

### 远程配置与指令

服务端设置 `security.allowAgentCommands` 默认关闭。启用后，远程配置要求 Agent 先上报受支持的配置；下发指令还要求机器在线并声明相应能力。

可同步的运行配置只有七项：`sample_interval_secs`、`report_interval_secs`、`include_nics`、`exclude_nics`、`include_mounts`、`exclude_mounts` 和 `auto_update`。接口使用 `_secs` 名称，本地 JSON 使用 `sample_interval` 和 `report_interval`，不要混写。

命令行参数和环境变量的显式覆盖仍然优先。与远程值冲突时，Agent 会拒绝整个版本，保留原运行配置，并回报冲突字段。请先修改服务参数或环境并重启，再重新检查远程配置。

可用的动作有 `re_report`、`check_update`、`restart` 和 `upgrade`。`restart` 和 `upgrade` 只允许在声明 `service_tier: A` 的托管安装中执行；手动安装会被拒绝，因为重启失败会让机器无法继续上报。这里没有任意 Shell 命令执行功能。

保存远程版本不等于已经应用。要等 Agent 上报确认，再到控制台查看执行记录和回执。

## 更新与卸载

### 更新

```sh
pa update
pa rollback
```

`pa update` 下载稳定版并校验文件完整性。托管安装可以在控制台主动开启自动更新；检查在成功上报后执行，每天最多一次，默认关闭。更新后查看 `pa status`、日志和控制台中的版本与最近上报时间。

更新下载使用固定的 GitHub HTTPS 地址，与 `endpoint` 使用 HTTP 还是 HTTPS 无关。`pa rollback` 使用本机保留的上一份二进制，不回滚服务端数据或配置。Docker 安装通过重建并替换容器更新。

OpenRC 安装需要 `supervise-daemon`，才能在控制台更新或重启后重新启动探针。已有服务若在更新后显示 `crashed`，请以 root 身份重新运行当前安装器，保持原来的模式、服务名和安装目录。默认系统安装可执行：

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.sh' -o /tmp/polaris-install.sh
sh /tmp/polaris-install.sh --mode system
pa status
```

重装会保留现有端点、Token 和本地设置；合并已有配置需要 Python 3。安装器会更新 OpenRC 服务，让它守护前台探针进程，退出后每隔 5 秒重新启动，不限制次数。仅更新二进制不会更新服务脚本。

### 卸载

可以直接用程序卸载自己，它会注销命令、停止并删除服务、程序文件和配置：

```sh
pa uninstall
```

系统安装要提权执行，例如 `sudo pa uninstall`；Windows 请用管理员 PowerShell。

先确认范围可以用 `pa uninstall --dry-run`，跳过确认用 `--yes`。停止受管理的服务会终止当前进程，所以确认后会交给脱离的辅助脚本收尾，日志写在程序旁的 `uninstall.log`。Docker 安装由宿主机上的 Compose 项目管理，程序内会拒绝，并提示在宿主机执行 `docker compose down`。

也可以用安装脚本卸载，权限、模式、服务名和安装目录要和安装时一致：

```sh
curl -fsSL 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.sh' -o install.sh
sh install.sh --uninstall
```

```powershell
Invoke-WebRequest 'https://raw.githubusercontent.com/XingLingQAQ/Polaris-Agent/main/install.ps1' -OutFile install.ps1
& ./install.ps1 -Uninstall
```

权限、模式、服务名和安装目录要和安装时一致。卸载器会先检查服务、程序、注册命令和配置的归属，再删除。卸载主机程序不会删除控制台的监控条目，后台删除条目也不会远程卸载机器程序。

## 数据与连接

连接和上报失败后会一直重试，直到服务停止。上报遇到网络错误时按指数退避并加入随机延迟，等待时间从约 5 秒增加到最多 60 秒。WebSocket 会话连续失败三次后切换到 HTTP，并持续检查 WebSocket 是否恢复。收到 401 或 403 时，Agent 保持运行，每次冷却五分钟后重试；请检查机器 Token 和访问规则。服务端返回 `Retry-After` 时会延长等待，最多一小时。

日志默认使用 `info` 级别，记录启动、注册、重试等待时间和恢复情况。每项操作连续失败时，每分钟最多输出一次告警；设置 `RUST_LOG=debug` 可查看逐次重试和上报确认。请在服务配置中设置环境变量并重启服务，再用 `pa logs -f` 查看进展。

- 负载、连接数和进程数是独立指标，不是百分比。Windows 的负载是平滑后的处理器队列长度，Linux 和 macOS 使用系统负载平均值，两者不能直接横向比较。
- 无法采集的指标显示 `—`，真实测到的零值显示 `0`。
- 待发送的快照和分钟摘要只存在内存里：最多 4096 条采样、60 帧快照（其中最多 30 帧可以领先服务端时间）和 128 个分钟桶。缓冲区满时会记录告警，并可能丢弃数据；异常退出会丢失未确认的数据。
- 超过 24 小时的上报会被确认并忽略。比服务端时间超前 5 分钟以上的上报会暂缓，直到时间进入允许范围。重复的分钟摘要按时间桶去重。
- 在线超时是上报间隔的三倍，下限 30 秒，上限 3600 秒。默认 60 秒上报时，超过约 180 秒没有上报即判为离线。
- 代理和防火墙需要放行 Agent API 和 WebSocket，机器要能解析并访问站点域名。
- Agent 通过真实网卡出口发现公网 IP，再查询国家、地区和 ISP。IP 查询不走 HTTP 环境代理，遇到 Fake-IP 时会重新解析查询服务的真实地址。Windows 会排除 Clash、Mihomo、Tailscale 等隧道网卡。无法确认时保留未知，不做猜测。
- ICMP 需要 raw socket 权限。没有权限时，Agent 会报告不支持这项检查，而不是静默失败。

## 常见问题

**控制台显示机器离线。** 在机器上执行 `pa status` 和 `pa logs -f`，确认能访问站点域名，并确认 Token 属于这台机器。Worker 后端默认每 60 秒上报一次，短暂中断可能要等约三分钟才会恢复。

**指标不对或者缺少指标。** 先检查网卡和挂载点过滤规则，再看系统实际能读到什么。容器只看到容器视图，除非显式挂载了宿主机资源。显示为 `—` 的值是没有采集到，不是零。

**找不到 `pa` 命令。** 先重新打开终端。仍然找不到时，使用安装时输出的包装器目录，或者用 `polaris-agent command register` 重新注册。

**添加机器后没有生成监控。** 登记机器和监控是两件事。需要新增 Agent 类型的监控并选择这台机器；机器页开关和监控的公开设置也是分开的。

**Agent 上的探测失败，但目标在别处正常。** 探测从这台机器的网络和权限发起。请检查路由、防火墙规则，ICMP 还要检查 raw socket 权限。

更多排查见 [Agent 手册](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/agents.md) 和 [常见问题](https://github.com/XingLingQAQ/UptimePolaris/blob/main/docs/troubleshooting.md)。

## 许可证

Uptime Polaris Agent 采用 [MIT 许可证](LICENSE)。
