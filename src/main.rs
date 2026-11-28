//! Supervise sampling and reporting sessions; switch to HTTP after repeated WebSocket failures and periodically retry WebSocket.
//! 管理采样和上报会话；WebSocket 多次失败后切换 HTTP，并定期重试 WebSocket。

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use tracing::{error, info, warn};
use clap::Parser;
use polaris_agent::commands::Signal;
use polaris_agent::config::{CliArgs, Config};
use polaris_agent::queue::FrameQueue;
use polaris_agent::rpc::{self, AgentClient, HttpTransport};
use polaris_agent::shutdown;
use polaris_agent::retry::{Retry, describe};
use polaris_agent::sysstat;

/// Wait after an unexpected failure. Long enough not to spin against a server
/// that is down, short enough that a transient fault costs little.
/// 异常失败后的等待时间：避免对故障服务器空转，同时限制瞬时故障的影响。
const RESTART_PAUSE: Duration = Duration::from_secs(5);

/// Failed WebSocket sessions before switching to HTTP.
/// 切换到 HTTP 前允许的 WebSocket 会话失败次数。
const WS_ATTEMPTS_BEFORE_FALLBACK: u32 = 3;

/// Interval for WebSocket recovery checks while reporting over HTTP.
/// HTTP 上报期间检查 WebSocket 恢复的间隔。
const WS_RETRY_INTERVAL: Duration = Duration::from_secs(300);
const MAX_SUMMARIES_PER_REPORT: usize = 32;
const AUTO_UPDATE_INTERVAL: Duration = Duration::from_secs(86_400);

/// Bound the wait for a restart command receipt before allowing the process to exit.
/// 限制退出前等待重启命令回执的时间。
const EXIT_DELIVERY_GRACE: Duration = Duration::from_secs(15);

enum SessionEnd {
    Ended,
    Recovered(AgentClient),
    Updated,
    /// A command requested exit; its receipt wait has completed or timed out.
    /// 命令请求退出，回执等待已完成或超时。
    Commanded,
    Reconfigured(polaris_agent::runtime_config::Reconfiguration),
}

struct UpdateSchedule {
    enabled: bool,
    last_attempt: Option<tokio::time::Instant>,
}

fn update_due(
    enabled: bool,
    last_attempt: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
) -> bool {
    enabled
        && last_attempt.map_or(true, |last| {
            now.duration_since(last) >= AUTO_UPDATE_INTERVAL
        })
}

fn update_should_exit(result: anyhow::Result<polaris_agent::updater::UpdateResult>) -> bool {
    matches!(result, Ok(polaris_agent::updater::UpdateResult::Applied | polaris_agent::updater::UpdateResult::RestartPending))
}

async fn check_auto_update(cfg: &Config, schedule: &mut UpdateSchedule) -> bool {
    let now = tokio::time::Instant::now();
    if !update_due(schedule.enabled, schedule.last_attempt, now) {
        return false;
    }
    schedule.last_attempt = Some(now);
    let update_config = cfg.clone();
    let result = tokio::task::spawn_blocking(move || {
        polaris_agent::updater::update_running(&update_config)
    })
    .await;
    match result {
        Ok(Ok(outcome)) => {
            if update_should_exit(Ok(outcome)) {
                info!("Agent update ready; exiting for the managed restart");
                true
            } else if outcome == polaris_agent::updater::UpdateResult::Staged {
                warn!("automatic update returned a manual staging result; keeping current process running");
                false
            } else {
                false
            }
        }
        Ok(Err(err)) => {
            warn!(err = %describe(&err), "automatic Agent update failed; next scheduled check in 24 hours");
            false
        }
        Err(err) => {
            warn!(err = %err, "automatic Agent update worker failed; next scheduled check in 24 hours");
            false
        }
    }
}

async fn recovery_loop<T, F, Fut>(
    delay: Duration,
    mut connect: F,
    sender: tokio::sync::mpsc::Sender<T>,
) where
    T: Send + 'static,
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut retry = Retry::new(delay, delay.saturating_mul(4));
    let mut pause = delay;
    loop {
        tokio::time::sleep(pause).await;
        match connect().await {
            Ok(client) => {
                let _ = sender.send(client).await;
                return;
            }
            Err(err) => pause = retry.fail(&err, "websocket recovery"),
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 通道关闭后保持挂起，避免 select! 重复选择已结束的信号源。
/// Stay pending after channel closure so select! cannot repeatedly select an exhausted signal source.
async fn next_signal(rx: &mut tokio::sync::mpsc::Receiver<Signal>) -> Signal {
    match rx.recv().await {
        Some(signal) => signal,
        None => std::future::pending().await,
    }
}

/// 退出前有限等待命令回执；超时或发送方关闭后仍继续退出。
/// Bound the wait for a command receipt before exit; continue after timeout or sender closure.
async fn wait_for_delivery(receipt: tokio::sync::oneshot::Receiver<()>) {
    if tokio::time::timeout(EXIT_DELIVERY_GRACE, receipt)
        .await
        .is_err()
    {
        warn!("command result was not acknowledged before the exit deadline");
    }
}

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(agent_main());
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn agent_main() -> anyhow::Result<()> {
    // Before anything that could block: a stop arriving during startup should
    // still be honoured rather than ignored until the first report.
    // 在可能阻塞的操作前安装处理器，及时响应启动期间的停止请求。
    shutdown::install();
    let args = CliArgs::parse();
    let Some(cfg) = polaris_agent::cli::dispatch(args)? else { return Ok(()); };
    polaris_agent::cli::initialize_logging(cfg.log_file.clone())?;
    let running = run_agent(cfg);
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result,
        _ = shutdown::wait() => {
            info!("stopping Agent; allowing up to five seconds for final reports");
            match tokio::time::timeout(Duration::from_secs(5), &mut running).await {
                Ok(result) => result,
                Err(_) => { warn!("shutdown deadline reached; unacknowledged data remains unsent"); Ok(()) }
            }
        }
    }
}

async fn run_agent(mut cfg: Config) -> anyhow::Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "starting polaris-agent");

    // The Worker batches a minute; the Go server expects every sample. Ask the
    // backend which it is before the first sample so the cadence is right from
    // the start; a failed probe keeps the default and must not stop the agent.
    // Worker 按分钟批量上报，Go 服务端要求逐样本上报。首次采样前先确认部署类型；
    // 探测失败时保留默认周期，且不得阻止 Agent 启动。
    let runtime = match polaris_agent::rpc::capabilities::discover(&cfg).await {
        Ok(runtime) => {
            info!(runtime = runtime.as_str(), "backend runtime discovered");
            Some(runtime)
        }
        Err(err) => {
            warn!(err = %describe(&err), "backend runtime discovery failed; keeping the default report interval");
            None
        }
    };
    polaris_agent::rpc::capabilities::apply_reporting_runtime(&mut cfg, runtime);

    info!(
        token_len = cfg.token.len(),
        sample_interval = cfg.sample_interval_secs,
        report_interval = cfg.report_interval_secs,
        version = env!("CARGO_PKG_VERSION"),
        "reporting cadence configured"
    );

    // One-shot extras: public IP / ISP / region / virtualization. Network
    // failures are silent by design - nulls get sent and the probe reports
    // fine. Collected once, not per reconnect, so a flapping link does not
    // re-hit the resolvers every few seconds.
    // 公网 IP、ISP、地区及虚拟化信息只采集一次；网络失败有意静默处理，发送 null 仍可正常上报。
    // 重连时不重新采集，避免网络反复中断时每隔几秒查询解析服务。
    let extras = sysstat::extras::collect().await;
    info!(
        public_ip = ?extras.public_ip,
        region = ?extras.region,
        virtualization = %extras.virtualization,
        "extras collected"
    );

    run_forever(cfg, extras).await
}

/// Which transport the loop is currently using.
/// 当前上报循环使用的传输方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    WebSocket,
    Http,
}

/// 跨传输会话保留采样器、最新快照与已完成的分钟汇总。
/// Retain the sampler, latest snapshot, and finalized minute summaries across transport sessions.
struct SamplingState {
    aggregator: sysstat::MinuteAggregator,
    finalized: VecDeque<sysstat::FinalizedAggregate>,
    latest: (sysstat::SysSnapshot, i64),
    samples: tokio::sync::mpsc::Receiver<(sysstat::SysSnapshot, i64)>,
    sampler_task: AbortOnDrop,
    hardware: sysstat::hardware::HardwareTelemetry,
}

fn start_sampler(mut sampler: sysstat::Sampler, interval_secs: u64, hardware: sysstat::hardware::HardwareReader) -> (tokio::sync::mpsc::Receiver<(sysstat::SysSnapshot, i64)>, AbortOnDrop) {
    start_sample_task(move || {
        let mut snapshot = sampler.capture();
        hardware.apply(&mut snapshot);
        (snapshot, observed_at())
    }, Duration::from_secs(interval_secs))
}

fn start_sample_task(mut capture: impl FnMut() -> (sysstat::SysSnapshot, i64) + Send + 'static, interval: Duration) -> (tokio::sync::mpsc::Receiver<(sysstat::SysSnapshot, i64)>, AbortOnDrop) {
    let (sender, receiver) = tokio::sync::mpsc::channel(4096);
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if sender.try_send(capture()).is_err() {
                warn!("sample buffer full or receiver closed; sample discarded");
            }
        }
    });
    (receiver, AbortOnDrop(task))
}

impl SamplingState {
    fn new(cfg: &Config) -> Self {
        let hardware = sysstat::hardware::HardwareTelemetry::start(cfg.collect_gpus, cfg.collect_sensors);
        let mut sampler = sysstat::Sampler::with_config(cfg.sample_interval_secs, cfg);
        let mut aggregator = sysstat::MinuteAggregator::default();
        let mut finalized = VecDeque::new();
        let latest = record_aggregate(sampler.capture(), observed_at(), &mut aggregator, &mut finalized);
        let (samples, sampler_task) = start_sampler(sampler, cfg.sample_interval_secs, hardware.reader());
        Self { aggregator, finalized, latest, samples, sampler_task, hardware }
    }

    async fn reconfigure(&mut self, cfg: &Config) {
        self.sampler_task.0.abort();
        let _ = (&mut self.sampler_task.0).await;
        drain_samples(&mut self.samples, &mut self.aggregator, &mut self.finalized, &mut self.latest);
        let sampler = sysstat::Sampler::with_config(cfg.sample_interval_secs, cfg);
        (self.samples, self.sampler_task) = start_sampler(sampler, cfg.sample_interval_secs, self.hardware.reader());
    }
}

fn drain_samples(samples: &mut tokio::sync::mpsc::Receiver<(sysstat::SysSnapshot, i64)>, aggregator: &mut sysstat::MinuteAggregator, finalized: &mut VecDeque<sysstat::FinalizedAggregate>, latest: &mut (sysstat::SysSnapshot, i64)) {
    while let Ok((snapshot, at)) = samples.try_recv() {
        *latest = record_aggregate(snapshot, at, aggregator, finalized);
    }
}

async fn stop_and_drain(sampler_task: &mut AbortOnDrop, samples: &mut tokio::sync::mpsc::Receiver<(sysstat::SysSnapshot, i64)>, aggregator: &mut sysstat::MinuteAggregator, finalized: &mut VecDeque<sysstat::FinalizedAggregate>, latest: &mut (sysstat::SysSnapshot, i64)) {
    sampler_task.0.abort();
    let _ = (&mut sampler_task.0).await;
    drain_samples(samples, aggregator, finalized, latest);
}

async fn run_forever(mut cfg: Config, extras: sysstat::AgentExtras) -> anyhow::Result<()> {
    // The probes task cannot send a report or end the process itself: the
    // transports live here, and so does the loop that decides when to leave.
    // 探针任务不能自行上报或退出；传输和退出决策由此循环负责。
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::channel(4);
    let probe_task = tokio::spawn(polaris_agent::probes::run(cfg.clone(), signal_tx));
    let _probe_task = AbortOnDrop(probe_task);
    let mut retry = Retry::new(RESTART_PAUSE, Duration::from_secs(60));
    let mut transport = Transport::WebSocket;
    let mut recovered_ws = None;
    let mut update_schedule = UpdateSchedule {
        enabled: cfg.auto_update && polaris_agent::runtime_config::auto_update_supported(&cfg),
        last_attempt: None,
    };
    if cfg.auto_update && !update_schedule.enabled {
        warn!("automatic Agent updates require a supervised installation outside containers");
    }

    // Keep undelivered frames across reporting sessions.
    // 跨上报会话保留未送达的帧。
    let mut pending = FrameQueue::default();
    let mut sampling = SamplingState::new(&cfg);

    // Built once. Rebuilding per attempt would re-parse the URL every five
    // seconds on a host that cannot reach us at all.
    // 只构建一次，避免完全无法连接时每五秒重试都重新解析 URL。
    let http = HttpTransport::new(&cfg)?;

    loop {
        if shutdown::requested() {
            info!("shutdown signalled - exiting cleanly");
            return Ok(());
        }

        let outcome = match transport {
            Transport::WebSocket => {
                let attempt = std::panic::AssertUnwindSafe(session_ws(
                    &cfg,
                    &extras,
                    &mut pending,
                    &mut sampling,
                    recovered_ws.take(),
                    &mut update_schedule,
                    &mut signal_rx,
                    &mut retry,
                ));
                match futures_util::FutureExt::catch_unwind(attempt).await {
                    Ok(r) => r,
                    Err(_) => {
                        error!("websocket session panicked");
                        Err(anyhow::anyhow!("panicked"))
                    }
                }
            }
            Transport::Http => {
                session_http(
                    &cfg,
                    &extras,
                    &http,
                    &mut pending,
                    &mut sampling,
                    &mut update_schedule,
                    &mut signal_rx,
                )
                .await
            }
        };

        if shutdown::requested() {
            info!("shutdown signalled - exiting cleanly");
            return Ok(());
        }

        let pause = match outcome {
            Ok(SessionEnd::Reconfigured(request)) => {
                let Some((next, applied)) = request.claim_and_commit() else {
                    continue;
                };
                cfg = *next;
                sampling.reconfigure(&cfg).await;
                update_schedule.enabled =
                    cfg.auto_update && polaris_agent::runtime_config::auto_update_supported(&cfg);
                recovered_ws = None;
                let _ = applied.send(Ok(()));
                continue;
            }
            Ok(SessionEnd::Updated) => return Ok(()),
            Ok(SessionEnd::Commanded) => return Ok(()),
            Ok(SessionEnd::Recovered(client)) => {
                recovered_ws = Some(client);
                retry.success("websocket");
                info!("websocket recovered; switching back");
                transport = Transport::WebSocket;
                continue;
            }
            Ok(SessionEnd::Ended) if transport == Transport::WebSocket => {
                // A clean end means the connection dropped, which is ordinary.
                // 正常结束表示连接断开，属于常见情况。
                retry.fail(&anyhow::anyhow!("WebSocket session ended"), "websocket")
            }
            Err(e) if transport == Transport::WebSocket => {
                let pause = retry.fail(&e, "websocket");
                let ws_failures = retry.failures();
                if ws_failures >= WS_ATTEMPTS_BEFORE_FALLBACK && !polaris_agent::retry::is_rejected(&e) {
                    warn!(
                        attempts = ws_failures,
                        "websocket is not coming up; falling back to HTTP POST"
                    );
                    transport = Transport::Http;
                    if !e.downcast_ref::<polaris_agent::retry::Rejection>().is_some_and(|rejected| rejected.retry_after.is_some()) {
                        continue;
                    }
                }
                pause
            }
            Ok(SessionEnd::Ended) => {
                RESTART_PAUSE
            }
            Err(e) => {
                retry.fail(&e, "http session")
            }
        };

        let deadline = tokio::time::Instant::now() + pause;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                Some((snapshot, at)) = sampling.samples.recv() => {
                    sampling.latest = record_aggregate(snapshot, at, &mut sampling.aggregator, &mut sampling.finalized);
                }
                signal = next_signal(&mut signal_rx) => match signal {
                    Signal::Reconfigure(request) => {
                        if let Some((next, applied)) = request.claim_and_commit() {
                            cfg = *next;
                            sampling.reconfigure(&cfg).await;
                            update_schedule.enabled = cfg.auto_update && polaris_agent::runtime_config::auto_update_supported(&cfg);
                            let _ = applied.send(Ok(()));
                        }
                    }
                    Signal::ReReport => {},
                    Signal::Exit(receipt) => { wait_for_delivery(receipt).await; return Ok(()); }
                },
                _ = shutdown::wait() => {
                    info!("shutdown signalled during backoff - exiting cleanly");
                    return Ok(());
                }
            }
        }
    }
}

/// One WebSocket connection's lifetime: connect, announce, flush anything the
/// previous session could not send, then report until it breaks.
/// 单次 WebSocket 会话：连接、声明身份、发送上一会话未能送达的帧，并持续上报直至连接中断。
async fn session_ws(
    cfg: &Config,
    extras: &sysstat::AgentExtras,
    pending: &mut FrameQueue,
    sampling: &mut SamplingState,
    recovered: Option<AgentClient>,
    update_schedule: &mut UpdateSchedule,
    signals: &mut tokio::sync::mpsc::Receiver<Signal>,
    retry: &mut Retry,
) -> anyhow::Result<SessionEnd> {
    let mut client = match recovered {
        Some(client) => client,
        None => AgentClient::connect(cfg).await?,
    };

    let basic = rpc::protocol::build_basic_report(extras, "ws", cfg.report_interval_secs)?;
    client.send(basic).await?;
    info!("basicReport sent, agent registered");

    // Announce current identity before flushing queued reports.
    // 发送积压上报前先声明当前身份。
    if !pending.is_empty() {
        let queued = pending.take();
        let count = queued.len();
        let mut deferred = Vec::new();
        let mut iter = queued.into_iter();
        while let Some(frame) = iter.next() {
            match client.send_status(frame.clone()).await {
                Ok(true) => {},
                Ok(false) => deferred.push(frame),
                Err(e) => {
                    deferred.push(frame);
                    deferred.extend(iter);
                    pending.restore_front(deferred);
                    return Err(e);
                }
            }
        }
        restore_deferred(pending, deferred);
        info!(frames = count, "replayed queued frames");
    }

    let SamplingState { aggregator, finalized, latest, samples, sampler_task, .. } = sampling;
    drain_samples(samples, aggregator, finalized, latest);
    let mut report_ticker = tokio::time::interval(Duration::from_secs(cfg.report_interval_secs));
    report_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    report_ticker.tick().await;
    let (initial_report, sent_summaries) = queued_report(latest, finalized);
    let delivered = match client.send_status(initial_report).await {
        Ok(true) => { acknowledge_summaries(finalized, sent_summaries); true },
        Ok(false) => { queue_snapshot(pending, latest); false },
        Err(err) => {
            queue_snapshot(pending, latest);
            return Err(err);
        }
    };
    if delivered { retry.success("websocket reports"); }
    if can_auto_update(delivered, pending, finalized) && samples.is_empty() && check_auto_update(cfg, update_schedule).await {
        stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
        flush_final_ws(&mut client, finalized, latest).await;
        return Ok(SessionEnd::Updated);
    }
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            Some((snapshot, at)) = samples.recv() => {
                *latest = record_aggregate(snapshot, at, aggregator, finalized);
                continue;
            }
            _ = report_ticker.tick() => {}
            frame = client.next_frame() => {
                if frame.is_none() {
                    anyhow::bail!("websocket closed by server");
                }
                continue;
            }
            _ = keepalive.tick() => {
                client.ping().await?;
                continue;
            }
            signal = next_signal(signals) => match signal {
                Signal::Reconfigure(request) => {
                    let (report, sent_summaries) = queued_report(latest, finalized);
                    if matches!(client.send_status(report).await, Ok(true)) {
                        acknowledge_summaries(finalized, sent_summaries);
                    } else {
                        queue_snapshot(pending, latest);
                    }
                    return Ok(SessionEnd::Reconfigured(request));
                }
                Signal::ReReport => {
                    let basic = rpc::protocol::build_basic_report(extras, "ws", cfg.report_interval_secs)?;
                    client.send(basic).await?;
                    info!("re_report command: sent a fresh basicReport");
                    continue;
                }
                Signal::Exit(receipt) => {
                    info!("restart command accepted; ending the session so the process can exit");
                    wait_for_delivery(receipt).await;
                    stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
                    flush_final_ws(&mut client, finalized, latest).await;
                    return Ok(SessionEnd::Commanded);
                }
            },
            _ = shutdown::wait() => {
                info!("shutdown signalled - ending session");
                stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
                flush_final_ws(&mut client, finalized, latest).await;
                return Ok(SessionEnd::Ended);
            }
        }

        let (report, sent_summaries) = queued_report(latest, finalized);

        let delivered = match client.send_status(report).await {
            Ok(true) => { acknowledge_summaries(finalized, sent_summaries); true },
            Ok(false) => { queue_snapshot(pending, latest); false },
            Err(e) => {
                queue_snapshot(pending, latest);
                return Err(e);
            }
        };
        if delivered { retry.success("websocket reports"); }
        if can_auto_update(delivered, pending, finalized) && samples.is_empty() && check_auto_update(cfg, update_schedule).await {
            stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
            flush_final_ws(&mut client, finalized, latest).await;
            return Ok(SessionEnd::Updated);
        }
    }
}

fn can_auto_update(delivered: bool, pending: &FrameQueue, finalized: &VecDeque<sysstat::FinalizedAggregate>) -> bool {
    delivered && pending.is_empty() && finalized.is_empty()
}

fn restore_deferred(pending: &mut FrameQueue, mut deferred: Vec<rpc::RpcRequest>) {
    let deferred_limit = polaris_agent::queue::DEFAULT_CAPACITY / 2;
    if deferred.len() > deferred_limit {
        let excess = deferred.len() - deferred_limit;
        deferred.drain(..excess);
        warn!(dropped = excess, "deferred report queue full; dropping oldest deferred frames");
    }
    pending.restore_front(deferred);
}

fn queue_snapshot(pending: &mut FrameQueue, latest: &(sysstat::SysSnapshot, i64)) {
    let frame = rpc::protocol::build_report(&latest.0, latest.1, &[]);
    if pending.push_reserving_prefix(frame, polaris_agent::queue::DEFAULT_CAPACITY / 2) {
        warn!("report queue full; replacing oldest unreserved snapshot");
    }
}

fn observed_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn record_aggregate(
    snapshot: sysstat::SysSnapshot,
    observed_at: i64,
    aggregator: &mut sysstat::MinuteAggregator,
    finalized: &mut VecDeque<sysstat::FinalizedAggregate>,
) -> (sysstat::SysSnapshot, i64) {
    let mem_pct = (snapshot.mem_total > 0)
        .then_some(snapshot.mem_used as f64 * 100.0 / snapshot.mem_total as f64);
    if let Some(bucket) = aggregator.push(
        observed_at,
        sysstat::AggregateSample {
            cpu: Some(snapshot.cpu_usage),
            mem_pct,
            load1: snapshot.load1,
            load5: snapshot.load5,
            net_up_total: Some(snapshot.net_total_up),
            net_down_total: Some(snapshot.net_total_down),
        },
    ) {
        if finalized.len() == 128 {
            warn!("summary buffer full; dropping oldest minute");
            finalized.pop_front();
        }
        finalized.push_back(bucket);
    }
    (snapshot, observed_at)
}

async fn flush_final_ws(client: &mut AgentClient, finalized: &mut VecDeque<sysstat::FinalizedAggregate>, latest: &(sysstat::SysSnapshot, i64)) {
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (report, count) = queued_report(latest, finalized);
            if !matches!(client.send_status(report).await, Ok(true)) { break; }
            if count.is_empty() { break; }
            acknowledge_summaries(finalized, count);
            if finalized.is_empty() { break; }
        }
    }).await;
}

async fn flush_final_http(http: &HttpTransport, pending: &mut FrameQueue, finalized: &mut VecDeque<sysstat::FinalizedAggregate>, latest: &(sysstat::SysSnapshot, i64)) {
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        if !pending.is_empty() {
            let queued = pending.take();
            match http.send_batch_status(&queued).await {
                Ok(statuses) => restore_deferred(pending, queued.into_iter().zip(statuses).filter_map(|(frame, delivered)| (!delivered).then_some(frame)).collect()),
                Err(_) => { pending.restore_front(queued); return; }
            }
        }
        loop {
            let (report, count) = queued_report(latest, finalized);
            if !matches!(http.send_status(&report).await, Ok(true)) { break; }
            if count.is_empty() { break; }
            acknowledge_summaries(finalized, count);
            if finalized.is_empty() { break; }
        }
    }).await;
}

fn queued_report(
    latest: &(sysstat::SysSnapshot, i64),
    finalized: &VecDeque<sysstat::FinalizedAggregate>,
) -> (rpc::RpcRequest, Vec<usize>) {
    let indices: Vec<_> = finalized.iter().enumerate()
        .filter(|(_, summary)| summary.bucket_ts <= latest.1)
        .take(MAX_SUMMARIES_PER_REPORT)
        .map(|(index, _)| index)
        .collect();
    let summaries: Vec<_> = indices.iter().map(|&index| finalized[index].clone()).collect();
    (rpc::protocol::build_report(&latest.0, latest.1, &summaries), indices)
}

fn acknowledge_summaries(finalized: &mut VecDeque<sysstat::FinalizedAggregate>, delivered: Vec<usize>) {
    for index in delivered.into_iter().rev() {
        finalized.remove(index);
    }
}

async fn session_http(
    cfg: &Config,
    extras: &sysstat::AgentExtras,
    http: &HttpTransport,
    pending: &mut FrameQueue,
    sampling: &mut SamplingState,
    update_schedule: &mut UpdateSchedule,
    signals: &mut tokio::sync::mpsc::Receiver<Signal>,
) -> anyhow::Result<SessionEnd> {
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel(1);
    let retry_cfg = cfg.clone();
    let retry_extras = extras.clone();
    let _retry_task = AbortOnDrop(tokio::spawn(recovery_loop(
        WS_RETRY_INTERVAL,
        move || {
            let cfg = retry_cfg.clone();
            let extras = retry_extras.clone();
            async move {
                let mut client = AgentClient::connect(&cfg).await?;
                client.send(rpc::protocol::build_basic_report(&extras, "ws", cfg.report_interval_secs)?).await?;
                Ok(client)
            }
        },
        ws_tx,
    )));
    let mut retry = Retry::new(RESTART_PAUSE, Duration::from_secs(60));
    let mut last_basic_report = None;
    let SamplingState { aggregator, finalized, latest, samples, sampler_task, .. } = sampling;
    drain_samples(samples, aggregator, finalized, latest);
    let mut report_ticker = tokio::time::interval(Duration::from_secs(cfg.report_interval_secs));
    report_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some((snapshot, at)) = samples.recv() => {
                *latest = record_aggregate(snapshot, at, aggregator, finalized);
                continue;
            }
            _ = report_ticker.tick() => { if !retry.ready() { continue; } }
            _ = tokio::time::sleep_until(retry.deadline()), if retry.failures() > 0 => {}
            Some(client) = ws_rx.recv() => return Ok(SessionEnd::Recovered(client)),
            _ = shutdown::wait() => {
                stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
                flush_final_http(http, pending, finalized, latest).await;
                return Ok(SessionEnd::Ended);
            }
            signal = next_signal(signals) => match signal {
                Signal::Reconfigure(request) => {
                    queue_snapshot(pending, latest);
                    return Ok(SessionEnd::Reconfigured(request));
                }
                Signal::ReReport => {
                    last_basic_report = None;
                    if retry.ready() { report_ticker.reset_immediately(); }
                    continue;
                }
                Signal::Exit(receipt) => {
                    info!("restart command accepted; waiting for its receipt before exit");
                    wait_for_delivery(receipt).await;
                    stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
                    flush_final_http(http, pending, finalized, latest).await;
                    return Ok(SessionEnd::Commanded);
                }
            },
        }

        if last_basic_report.is_none_or(|at: tokio::time::Instant| at.elapsed() >= Duration::from_secs(86_400)) {
            let basic = rpc::protocol::build_basic_report(extras, "http", cfg.report_interval_secs)?;
            match http.send(&basic).await {
                Ok(()) => {
                    last_basic_report = Some(tokio::time::Instant::now());
                    info!("basicReport posted over HTTP, agent registered");
                }
                Err(error) => {
                    retry.fail(&error, "HTTP registration");
                    queue_snapshot(pending, latest);
                    continue;
                }
            }
        }

        let (report, sent_summaries) = queued_report(latest, finalized);
        let mut queued = pending.take();
        queued.push(report);
        match http.send_batch_status(&queued).await {
            Ok(mut statuses) => {
                let report_delivered = statuses.pop().unwrap_or(false);
                queued.pop();
                let deferred: Vec<_> = queued.into_iter().zip(statuses).filter_map(|(frame, delivered)| (!delivered).then_some(frame)).collect();
                restore_deferred(pending, deferred);
                if report_delivered {
                    acknowledge_summaries(finalized, sent_summaries);
                } else {
                    queue_snapshot(pending, latest);
                }
                retry.success("HTTP reports");
                report_ticker.reset();
                if can_auto_update(report_delivered, pending, finalized) && samples.is_empty() && check_auto_update(cfg, update_schedule).await {
                    stop_and_drain(sampler_task, samples, aggregator, finalized, latest).await;
                    flush_final_http(http, pending, finalized, latest).await;
                    return Ok(SessionEnd::Updated);
                }
            }
            Err(error) => {
                queued.pop();
                pending.restore_front(queued);
                queue_snapshot(pending, latest);
                retry.fail(&error, "HTTP reports");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_session_keeps_pending_frames_until_recovery() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = Config {
            endpoint: format!("http://{}", listener.local_addr().unwrap()), token: "fixture".into(),
            sample_interval_secs: 1, report_interval_secs: 1, report_interval_explicit: true,
            insecure: false, include_nics: vec![], exclude_nics: vec![], include_mounts: vec![], exclude_mounts: vec![],
            auto_update: false, log_file: None, service_tier: None, config_path: "unused".into(),
            runtime_overrides: Default::default(), collect_gpus: false, collect_sensors: false,
        };
        let (signals, mut signal_rx) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(async move {
            let mut failed_at = None;
            let mut delivered = false;
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (header_end, length) = loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length: usize = header.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let value: serde_json::Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let frames = value.as_array().cloned().unwrap_or_else(|| vec![value]);
                if index == 1 {
                    failed_at = Some(tokio::time::Instant::now());
                    stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                if index == 2 {
                    assert!(failed_at.unwrap().elapsed() >= Duration::from_secs(4));
                    assert!(frames.iter().any(|frame| frame["params"]["sequence"] == 42));
                    delivered = true;
                }
                let acks: Vec<_> = frames.iter().map(|frame| serde_json::json!({"id":frame["id"],"ack":true,"ignored":false})).collect();
                let body = serde_json::json!({"ok":true,"applied":acks.len(),"acks":acks}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                if index == 2 {
                    let (sent, received) = tokio::sync::oneshot::channel();
                    sent.send(()).unwrap();
                    signals.send(Signal::Exit(received)).await.unwrap();
                }
            }
            assert!(delivered);
        });
        let mut pending = FrameQueue::default();
        pending.push(rpc::RpcRequest::notification("report", serde_json::json!({"sequence":42})));
        let mut sampling = SamplingState::new(&cfg);
        let mut schedule = UpdateSchedule { enabled: false, last_attempt: None };
        let transport = HttpTransport::new(&cfg).unwrap();
        let extras = sysstat::AgentExtras::fallback();
        let result = tokio::time::timeout(Duration::from_secs(12), session_http(&cfg, &extras, &transport,
            &mut pending, &mut sampling, &mut schedule, &mut signal_rx)).await.unwrap().unwrap();
        assert!(matches!(result, SessionEnd::Commanded));
        assert!(pending.is_empty());
        server.await.unwrap();
    }

    fn summary(bucket_ts: i64) -> sysstat::FinalizedAggregate {
        sysstat::FinalizedAggregate {
            bucket_ts,
            sample_count: 12,
            cpu_samples: 12,
            cpu_avg: Some(25.0),
            cpu_max: Some(50.0),
            mem_samples: 12,
            mem_pct_avg: Some(40.0),
            mem_pct_max: Some(60.0),
            load_1_avg: Some(0.5),
            load_5_avg: Some(0.7),
            net_up_bytes: Some(500),
            net_down_bytes: Some(800),
            network_duration_sec: 55,
            duration_sec: 55,
        }
    }

    #[test]
    fn prolonged_outage_reports_replay_payloads_in_bounded_exact_once_chunks() {
        let mut finalized: VecDeque<_> = (0..128).map(|minute| summary(minute * 60)).collect();
        let latest = (sysstat::SysSnapshot::default(), 10_000);
        let (failed, _) = queued_report(&latest, &finalized);
        let (retry, _) = queued_report(&latest, &finalized);
        assert_eq!(
            serde_json::to_value(failed).unwrap(),
            serde_json::to_value(retry).unwrap()
        );

        let mut delivered = Vec::new();
        while !finalized.is_empty() {
            let (frame, count) = queued_report(&latest, &finalized);
            let summaries = frame.params["summaries"].as_array().unwrap();
            assert!(!summaries.is_empty() && summaries.len() <= MAX_SUMMARIES_PER_REPORT);
            delivered.extend(
                summaries
                    .iter()
                    .map(|row| row["bucket_ts"].as_i64().unwrap()),
            );
            acknowledge_summaries(&mut finalized, count);
        }
        assert_eq!(
            delivered,
            (0..128).map(|minute| minute * 60).collect::<Vec<_>>()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sampling_continues_while_transport_waits_for_ack() {
        let mut n = 0;
        let (mut samples, _task) = start_sample_task(move || {
            n += 1;
            (sysstat::SysSnapshot::default(), 55 + n * 5)
        }, Duration::from_secs(5));
        tokio::task::yield_now().await;
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
        }
        let mut aggregator = sysstat::MinuteAggregator::default();
        let mut finalized = VecDeque::new();
        let mut latest = (sysstat::SysSnapshot::default(), 55);
        drain_samples(&mut samples, &mut aggregator, &mut finalized, &mut latest);
        assert_eq!(latest.1, 70);
        assert_eq!(finalized.len(), 0);
        let (snapshot, at) = (sysstat::SysSnapshot::default(), 120);
        latest = record_aggregate(snapshot, at, &mut aggregator, &mut finalized);
        assert_eq!(latest.1, 120);
        assert_eq!(finalized[0].bucket_ts, 60);
        assert_eq!(finalized[0].sample_count, 3);
    }

    #[test]
    fn future_summaries_wait_without_blocking_current_minutes() {
        let mut finalized = VecDeque::from(vec![summary(365 * 86_400), summary(120), summary(180)]);
        let latest = (sysstat::SysSnapshot::default(), 180);
        let (frame, indices) = queued_report(&latest, &finalized);
        let buckets: Vec<_> = frame.params["summaries"].as_array().unwrap().iter().map(|s| s["bucket_ts"].as_i64().unwrap()).collect();
        assert_eq!(buckets, vec![120, 180]);
        acknowledge_summaries(&mut finalized, indices);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].bucket_ts, 365 * 86_400);
    }

    #[test]
    fn deferred_capacity_reserves_space_for_current_failed_report() {
        let mut pending = FrameQueue::default();
        let frames = (0..60).map(|n| rpc::RpcRequest::notification("report", serde_json::json!({"sequence": n}))).collect();
        restore_deferred(&mut pending, frames);
        assert_eq!(pending.len(), 30);
        let latest = (sysstat::SysSnapshot::default(), 120);
        queue_snapshot(&mut pending, &latest);
        queue_snapshot(&mut pending, &latest);
        for _ in 0..28 { queue_snapshot(&mut pending, &latest); }
        assert_eq!(pending.len(), 60);
        queue_snapshot(&mut pending, &latest);
        let frames = pending.take();
        assert_eq!(frames.len(), 60);
        assert_eq!(frames[0].params["sequence"], 30);
        assert_eq!(frames[29].params["sequence"], 59);
        assert_eq!(frames[30].params["observed_at"], 120);
        assert_eq!(frames[59].params["observed_at"], 120);
    }

    #[test]
    fn auto_update_waits_for_acknowledgement_and_backlog() {
        let mut pending = FrameQueue::default();
        let mut finalized = VecDeque::new();
        assert!(!can_auto_update(false, &pending, &finalized));
        assert!(can_auto_update(true, &pending, &finalized));
        pending.push(rpc::RpcRequest::notification("report", serde_json::json!({})));
        assert!(!can_auto_update(true, &pending, &finalized));
        pending.take();
        finalized.push_back(summary(120));
        assert!(!can_auto_update(true, &pending, &finalized));
    }

    #[test]
    fn transport_switch_preserves_open_minute_and_unacknowledged_summaries() {
        let sample = |up| sysstat::AggregateSample {
            cpu: Some(30.0), mem_pct: Some(40.0), load1: Some(0.5), load5: Some(0.7),
            net_up_total: Some(up), net_down_total: Some(up * 2),
        };
        let mut aggregator = sysstat::MinuteAggregator::default();
        let mut finalized = VecDeque::new();
        assert!(aggregator.push(55, sample(100)).is_none());
        // WebSocket disconnect, then HTTP takes ownership of the same state.
        // WebSocket 断开后，HTTP 接管同一状态。
        finalized.push_back(aggregator.push(60, sample(110)).unwrap());
        let latest = (sysstat::SysSnapshot::default(), 60);
        let (failed_http, _) = queued_report(&latest, &finalized);
        let (retry_ws, count) = queued_report(&latest, &finalized);
        assert_eq!(serde_json::to_value(failed_http).unwrap(), serde_json::to_value(&retry_ws).unwrap());
        assert_eq!(retry_ws.params["summaries"][0]["bucket_ts"], 0);
        acknowledge_summaries(&mut finalized, count);
        assert!(finalized.is_empty());
        let next = aggregator.push(120, sample(120)).unwrap();
        assert_eq!(next.bucket_ts, 60);
        assert_eq!(next.net_up_bytes, Some(10));
        finalized.push_back(next);
        let (report, count) = queued_report(&latest, &finalized);
        assert_eq!(report.params["summaries"].as_array().unwrap().len(), 1);
        assert_eq!(report.params["summaries"][0]["bucket_ts"], 60);
        acknowledge_summaries(&mut finalized, count);
        assert!(finalized.is_empty());
    }

    #[tokio::test]
    async fn websocket_replay_retains_deferred_and_sends_later_frames() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            for index in 0..4 {
                let request: serde_json::Value = match ws.next().await.unwrap().unwrap() {
                    Message::Text(text) => serde_json::from_str(&text).unwrap(),
                    frame => panic!("unexpected frame: {frame:?}"),
                };
                if index == 1 { assert_eq!(request["params"]["sequence"], 1); }
                if index == 2 { assert_eq!(request["params"]["sequence"], 2); }
                ws.send(Message::Text(serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"ack":true,"ignored":false,"deferred":index == 1}}).to_string())).await.unwrap();
            }
            ws.close(None).await.unwrap();
        });
        let cfg = Config {
            endpoint: format!("ws://{address}"), token: "fixture".into(), sample_interval_secs: 5,
            report_interval_secs: 60, report_interval_explicit: false, insecure: false, include_nics: vec![], exclude_nics: vec![],
            include_mounts: vec![], exclude_mounts: vec![], auto_update: false,
            log_file: None, service_tier: None, config_path: std::path::PathBuf::from("unused"),
            runtime_overrides: Default::default(),
            collect_gpus: false, collect_sensors: false,
        };
        let mut pending = FrameQueue::default();
        for sequence in 1..=2 { pending.push(rpc::RpcRequest::notification("report", serde_json::json!({"sequence":sequence}))); }
        let mut schedule = UpdateSchedule { enabled: false, last_attempt: None };
        let (_sender, mut signals) = tokio::sync::mpsc::channel(1);
        let _ = session_ws(&cfg, &sysstat::AgentExtras::fallback(), &mut pending, &mut SamplingState::new(&cfg), None, &mut schedule, &mut signals, &mut Retry::new(RESTART_PAUSE, Duration::from_secs(60))).await;
        let queued = pending.take();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].params["sequence"], 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_flush_keeps_failed_and_unsent_frames() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            for index in 0..3 {
                let request: serde_json::Value = match ws.next().await.unwrap().unwrap() {
                    Message::Text(text) => serde_json::from_str(&text).unwrap(),
                    frame => panic!("unexpected frame: {frame:?}"),
                };
                if index == 2 {
                    assert_eq!(request["params"]["sequence"], 2);
                    ws.close(None).await.unwrap();
                    break;
                }
                ws.send(Message::Text(
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "result": { "ack": true, "ignored": false }
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            }
        });
        let cfg = Config {
            endpoint: format!("ws://{address}"),
            token: "fixture".into(),
            sample_interval_secs: 5,
            report_interval_secs: 60,
            report_interval_explicit: false,
            insecure: false,
            include_nics: vec![],
            exclude_nics: vec![],
            include_mounts: vec![],
            exclude_mounts: vec![],
            auto_update: false,
            log_file: None,
            service_tier: None,

            config_path: std::path::PathBuf::from("unused"),
            runtime_overrides: Default::default(),
            collect_gpus: false, collect_sensors: false,
        };
        let frames: Vec<_> = (1..=3)
            .map(|sequence| {
                rpc::RpcRequest::notification(
                    "report",
                    serde_json::json!({
                        "sequence": sequence, "summaries": [summary(sequence * 60)]
                    }),
                )
            })
            .collect();
        let expected = serde_json::to_value(&frames[1..]).unwrap();
        let mut pending = FrameQueue::default();
        for frame in frames {
            pending.push(frame);
        }
        let mut update_schedule = UpdateSchedule {
            enabled: false,
            last_attempt: None,
        };
        let (_sender, mut signals) = tokio::sync::mpsc::channel(1);
        let result = session_ws(
            &cfg,
            &sysstat::AgentExtras::fallback(),
            &mut pending,
            &mut SamplingState::new(&cfg),
            None,
            &mut update_schedule,
            &mut signals,
            &mut Retry::new(RESTART_PAUSE, Duration::from_secs(60)),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(serde_json::to_value(pending.take()).unwrap(), expected);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn recovery_task_is_cancelled_when_its_session_ends() {
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let owner = AbortOnDrop(task);
        drop(owner);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    #[tokio::test]
    async fn recovery_retries_on_the_injected_schedule_until_connected() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_count = attempts.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(recovery_loop(
            Duration::from_millis(5),
            move || {
                let n = attempt_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        anyhow::bail!("temporary failure");
                    }
                    Ok(17)
                }
            },
            tx,
        ));

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap(),
            Some(17)
        );
        task.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn automatic_update_is_checked_at_most_once_per_day_after_a_report() {
        let now = tokio::time::Instant::now();
        assert!(update_due(true, None, now));
        assert!(!update_due(false, None, now));
        assert!(!update_due(
            true,
            Some(now),
            now + Duration::from_secs(86_399)
        ));
        assert!(update_due(true, Some(now), now + AUTO_UPDATE_INTERVAL));
    }

    #[test]
    fn only_an_applied_or_handed_off_update_exits_for_restart() {
        assert!(update_should_exit(Ok(
            polaris_agent::updater::UpdateResult::Applied
        )));
        assert!(update_should_exit(Ok(polaris_agent::updater::UpdateResult::RestartPending)));
        assert!(!update_should_exit(Ok(
            polaris_agent::updater::UpdateResult::AlreadyCurrent
        )));
        assert!(!update_should_exit(Ok(
            polaris_agent::updater::UpdateResult::Staged
        )));
        assert!(!update_should_exit(Err(anyhow::anyhow!("fixture failure"))));
    }
}
