use super::Reading;
use anyhow::{ensure, Context, Result};
use futures_util::{stream, StreamExt};
use reqwest::{Client, Url};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};
use tokio::{
    sync::watch,
    time::{timeout_at, Instant},
};

const MAX_CONTAINERS: usize = 256;
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const POLL_INTERVAL: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContainerStats {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub runtime: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_sec: Option<u64>,
}

#[derive(Clone)]
enum Endpoint {
    Unix(PathBuf),
    Http(Url),
}

impl Endpoint {
    fn parse(value: &str) -> Result<Self> {
        ensure!(
            value.len() <= 4096 && !value.chars().any(char::is_control),
            "invalid container socket"
        );
        if value.starts_with('/') {
            return Ok(Self::Unix(PathBuf::from(value)));
        }
        let url = Url::parse(value)
            .context("container socket must be an absolute Unix path or HTTP origin")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "container proxy must be an HTTP origin without credentials, query or path"
        );
        Ok(Self::Http(url))
    }

    fn client(&self) -> Result<(Client, Url, &'static str)> {
        let builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .timeout(TOTAL_TIMEOUT);
        match self {
            Self::Http(url) => Ok((builder.build()?, url.clone(), "docker")),
            Self::Unix(path) => {
                #[cfg(unix)]
                {
                    Ok((
                        builder.unix_socket(path.clone()).build()?,
                        Url::parse("http://localhost/")?,
                        if path.to_string_lossy().contains("podman") {
                            "podman"
                        } else {
                            "docker"
                        },
                    ))
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    anyhow::bail!("Unix container sockets are unavailable on this platform; configure an HTTP proxy")
                }
            }
        }
    }
}

pub fn validate_socket(value: &str) -> Result<()> {
    Endpoint::parse(value).map(|_| ())
}

fn default_endpoints() -> Vec<Endpoint> {
    #[cfg(unix)]
    {
        let mut paths = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(root) = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            paths.push(root.join("docker.sock"));
            paths.push(root.join("podman/podman.sock"));
        }
        paths.push(PathBuf::from("/run/podman/podman.sock"));
        paths.into_iter().map(Endpoint::Unix).collect()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[derive(Clone, Copy)]
struct CpuSample {
    total: u64,
    system: u64,
}

struct Engine {
    client: Client,
    base: Url,
    runtime: &'static str,
    previous: HashMap<String, CpuSample>,
}

impl Engine {
    fn new(endpoint: &Endpoint) -> Result<Self> {
        let (client, base, runtime) = endpoint.client()?;
        Ok(Self {
            client,
            base,
            runtime,
            previous: HashMap::new(),
        })
    }

    async fn sample(&mut self, deadline: Instant) -> Result<Vec<ContainerStats>> {
        let mut url = self.base.join("containers/json")?;
        url.set_query(Some("all=1"));
        let (value, podman) = get_json(&self.client, url, deadline).await?;
        if podman {
            self.runtime = "podman";
        }
        let mut rows = parse_list(&value, self.runtime)?;
        let work: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.state == "running")
            .map(|(index, row)| (index, row.id.clone(), self.previous.get(&row.id).copied()))
            .collect();
        let client = &self.client;
        let base = &self.base;
        let mut tasks = stream::iter(work)
            .map(|(index, id, previous)| async move {
                let mut url = base.clone();
                url.path_segments_mut()
                    .expect("HTTP base")
                    .extend(["containers", &id, "stats"]);
                url.set_query(Some("stream=false&one-shot=true"));
                (index, previous, get_json(client, url, deadline).await)
            })
            .buffer_unordered(4);
        let mut next_cpu = HashMap::new();
        while let Ok(Some((index, previous, result))) = timeout_at(deadline, tasks.next()).await {
            if let Ok((stats, _)) = result {
                if let Some(sample) = apply_stats(&mut rows[index], &stats, previous) {
                    next_cpu.insert(rows[index].id.clone(), sample);
                }
            }
        }
        self.previous = next_cpu;
        Ok(rows)
    }
}

async fn get_json(client: &Client, url: Url, deadline: Instant) -> Result<(Value, bool)> {
    timeout_at(deadline, async {
        let response = client.get(url).send().await?;
        ensure!(
            response.status().is_success(),
            "container engine returned an unsuccessful status"
        );
        let podman = response.headers().contains_key("libpod-api-version");
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= MAX_BODY as u64),
            "container response exceeds limit"
        );
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len() + chunk.len() <= MAX_BODY,
                "container response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((serde_json::from_slice(&bytes)?, podman))
    })
    .await
    .context("container collection timed out")?
}

pub(super) async fn run(socket: Option<String>, tx: watch::Sender<Reading<ContainerStats>>) {
    let endpoints = match socket {
        Some(value) => match Endpoint::parse(&value) {
            Ok(endpoint) => vec![endpoint],
            Err(_) => return,
        },
        None => default_endpoints(),
    };
    if endpoints.is_empty() {
        return;
    }
    let mut engine: Option<Engine> = None;
    loop {
        let started = Instant::now();
        let deadline = started + TOTAL_TIMEOUT;
        let result = if let Some(engine) = &mut engine {
            engine.sample(deadline).await
        } else {
            let mut result = Err(anyhow::anyhow!("no readable container engine"));
            for endpoint in &endpoints {
                let Ok(mut candidate) = Engine::new(endpoint) else {
                    continue;
                };
                result = candidate.sample(deadline).await;
                if result.is_ok() {
                    engine = Some(candidate);
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            result
        };
        let delay = match result {
            Ok(rows) => {
                tx.send_modify(|reading| reading.update(rows));
                POLL_INTERVAL.saturating_sub(started.elapsed())
            }
            Err(_) => {
                tracing::debug!("container telemetry unavailable; retrying in 60 seconds");
                engine = None;
                RETRY_INTERVAL
            }
        };
        tokio::select! { _ = tx.closed() => return, _ = tokio::time::sleep(delay) => {} }
    }
}

fn text(value: &Value, max: usize) -> Option<String> {
    let value = value.as_str()?;
    if value.chars().any(char::is_control) {
        return None;
    }
    let name: String = value
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .chars()
        .take(max)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn integer(value: &Value, max: u64) -> Option<u64> {
    value.as_u64().filter(|n| *n <= max)
}

fn parse_list(value: &Value, runtime: &str) -> Result<Vec<ContainerStats>> {
    let input = value.as_array().context("invalid container list")?;
    let mut rows = Vec::new();
    let mut ids = HashSet::new();
    for item in input {
        let Some(id) = text(&item["Id"], 64).filter(|_| {
            item["Id"]
                .as_str()
                .is_some_and(|raw| raw.chars().count() <= 64)
        }) else {
            continue;
        };
        if !ids.insert(id.clone()) {
            continue;
        }
        let name = item["Names"]
            .as_array()
            .and_then(|names| names.first())
            .and_then(Value::as_str)
            .map(|name| Value::String(name.trim_start_matches('/').to_owned()))
            .and_then(|value| text(&value, 128))
            .or_else(|| text(&item["Name"], 128))
            .unwrap_or_else(|| id.clone());
        let state = match item["State"].as_str().unwrap_or("unknown") {
            state @ ("running" | "exited" | "restarting" | "paused" | "dead" | "created") => state,
            _ => "unknown",
        };
        let status = item["Status"].as_str().unwrap_or_default();
        let health = item["Health"]
            .as_str()
            .filter(|s| ["healthy", "unhealthy", "starting", "none"].contains(s))
            .or_else(|| {
                if status.contains("(unhealthy)") {
                    Some("unhealthy")
                } else if status.contains("(healthy)") {
                    Some("healthy")
                } else if status.contains("(health: starting)") {
                    Some("starting")
                } else {
                    None
                }
            });
        let uptime_sec = if state == "running" {
            integer(&item["StartedAt"], u64::MAX).and_then(|start| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                now.checked_sub(start)
                    .filter(|value| start > 0 && *value <= 3_153_600_000)
            })
        } else {
            None
        };
        rows.push(ContainerStats {
            id,
            name,
            image: text(&item["Image"], 256),
            runtime: runtime.to_owned(),
            state: state.to_owned(),
            health: health.map(str::to_owned),
            cpu_pct: None,
            mem_used_bytes: None,
            mem_limit_bytes: None,
            restart_count: integer(&item["RestartCount"], 1_000_000),
            uptime_sec,
        });
        if rows.len() == MAX_CONTAINERS {
            break;
        }
    }
    Ok(rows)
}

fn cpu_sample(value: &Value) -> Option<CpuSample> {
    Some(CpuSample {
        total: value["cpu_usage"]["total_usage"].as_u64()?,
        system: value["system_cpu_usage"].as_u64()?,
    })
}

fn apply_stats(
    row: &mut ContainerStats,
    value: &Value,
    previous: Option<CpuSample>,
) -> Option<CpuSample> {
    let mem = &value["memory_stats"];
    row.mem_limit_bytes = integer(&mem["limit"], MAX_SAFE_INTEGER);
    row.mem_used_bytes = integer(&mem["usage"], MAX_SAFE_INTEGER).map(|used| {
        let cache = integer(&mem["stats"]["total_inactive_file"], MAX_SAFE_INTEGER)
            .or_else(|| integer(&mem["stats"]["inactive_file"], MAX_SAFE_INTEGER))
            .unwrap_or(0);
        if cache <= used {
            used - cache
        } else {
            used
        }
    });
    let current = cpu_sample(&value["cpu_stats"])?;
    let before = cpu_sample(&value["precpu_stats"])
        .filter(|sample| sample.system > 0)
        .or(previous);
    let cpus = integer(&value["cpu_stats"]["online_cpus"], 1000)
        .filter(|n| *n > 0)
        .or_else(|| {
            value["cpu_stats"]["cpu_usage"]["percpu_usage"]
                .as_array()
                .map(|values| values.len() as u64)
                .filter(|count| (1..=1000).contains(count))
        });
    if let (Some(before), Some(cpus)) = (before, cpus) {
        if let (Some(total), Some(system)) = (
            current.total.checked_sub(before.total),
            current.system.checked_sub(before.system).filter(|n| *n > 0),
        ) {
            let pct = total as f64 / system as f64 * cpus as f64 * 100.0;
            row.cpu_pct = (pct.is_finite() && (0.0..=100_000.0).contains(&pct)).then_some(pct);
        }
    }
    Some(current)
}

#[cfg(test)]
#[path = "containers_tests.rs"]
mod tests;
