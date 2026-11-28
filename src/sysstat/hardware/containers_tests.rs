use super::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn row(id: &str) -> Value {
    json!({"Id":id,"Names":["/app"],"Image":"registry/app:v1","State":"running","Status":"Up 1 hour (healthy)"})
}
fn stats() -> Value {
    json!({"cpu_stats":{"cpu_usage":{"total_usage":200},"system_cpu_usage":2000,"online_cpus":4},"precpu_stats":{"cpu_usage":{"total_usage":100},"system_cpu_usage":1000},"memory_stats":{"usage":4096,"limit":8192,"stats":{"inactive_file":1024}}})
}

#[test]
fn list_bounds_and_optional_wire_fields() {
    let mut input: Vec<_> = (0..300).map(|i| row(&format!("id{i}"))).collect();
    input[0]["Names"] = json!([format!("/{}", "容".repeat(200))]);
    input[0]["Image"] = json!("像".repeat(300));
    input[0]["Env"] = json!(["SECRET=value"]);
    input[0]["Mounts"] = json!(["/private"]);
    let rows = parse_list(&json!(input), "docker").unwrap();
    assert_eq!(rows.len(), 256);
    assert_eq!(rows[0].name.chars().count(), 128);
    assert_eq!(rows[0].image.as_ref().unwrap().chars().count(), 256);
    let snapshot = crate::sysstat::SysSnapshot {
        containers: Some(rows),
        ..Default::default()
    };
    let frame =
        serde_json::to_value(crate::rpc::protocol::build_report(&snapshot, 1, &[])).unwrap();
    let serialized = frame.to_string();
    assert!(!serialized.contains("SECRET") && !serialized.contains("Mounts"));
    assert_eq!(frame["params"]["containers"][0]["runtime"], "docker");
    assert!(frame["params"]["containers"][0].get("cpu_pct").is_none());
    assert!(frame["params"]["containers"][0]
        .get("restart_count")
        .is_none());
    assert!(parse_list(&json!({}), "docker").is_err());
    assert_eq!(text(&json!("\u{feff}"), 128), None);
    assert_eq!(text(&json!("app\n"), 128), None);
    let mut invalid = row("bad\u{7f}");
    invalid["State"] = json!("other");
    assert!(parse_list(&json!([invalid]), "docker").unwrap().is_empty());
    let rows = parse_list(
        &json!([row("same"), row("same"), row(&"a".repeat(65))]),
        "podman",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn states_health_and_real_start_time_are_preserved() {
    for state in [
        "running",
        "exited",
        "restarting",
        "paused",
        "dead",
        "created",
        "unknown",
        "unsupported",
    ] {
        let mut input = row("id");
        input["State"] = json!(state);
        input["Status"] = json!("Up (unhealthy)");
        input["RestartCount"] = json!(2);
        input["Created"] = json!(1);
        let rows = parse_list(&json!([input]), "podman").unwrap();
        assert_eq!(
            rows[0].state,
            if state == "unsupported" {
                "unknown"
            } else {
                state
            }
        );
        assert_eq!(rows[0].health.as_deref(), Some("unhealthy"));
        assert_eq!(rows[0].restart_count, Some(2));
        assert_eq!(rows[0].uptime_sec, None);
    }
    let mut input = row("id");
    input["Status"] = json!("Up (health: starting)");
    input["StartedAt"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 10
    );
    let rows = parse_list(&json!([input]), "docker").unwrap();
    assert_eq!(rows[0].health.as_deref(), Some("starting"));
    assert!((10..=11).contains(&rows[0].uptime_sec.unwrap()));
}

#[test]
fn stats_use_multicore_deltas_and_reject_resets_or_unsafe_memory() {
    let mut container = parse_list(&json!([row("id")]), "docker").unwrap().remove(0);
    let current = apply_stats(&mut container, &stats(), None).unwrap();
    assert_eq!(container.cpu_pct, Some(40.0));
    assert_eq!(container.mem_used_bytes, Some(3072));
    assert_eq!(container.mem_limit_bytes, Some(8192));
    let mut next = stats();
    next["precpu_stats"] = json!({});
    next["cpu_stats"]["cpu_usage"]["total_usage"] = json!(800);
    next["cpu_stats"]["system_cpu_usage"] = json!(3000);
    apply_stats(&mut container, &next, Some(current));
    assert_eq!(container.cpu_pct, Some(240.0));
    container.cpu_pct = None;
    next["cpu_stats"]["cpu_usage"]["total_usage"] = json!(1);
    next["memory_stats"]["usage"] = json!(9007199254740992u64);
    apply_stats(&mut container, &next, Some(current));
    assert_eq!(container.cpu_pct, None);
    assert_eq!(container.mem_used_bytes, None);
}

#[test]
fn endpoints_reject_credentials_paths_redirect_schemes_and_relative_sockets() {
    for value in [
        "http://127.0.0.1:2375",
        "https://proxy.example",
        "/var/run/docker.sock",
        "/run/user/1000/podman/podman.sock",
    ] {
        assert!(validate_socket(value).is_ok(), "{value}");
    }
    for value in [
        "docker.sock",
        "tcp://localhost:2375",
        "file:///etc/passwd",
        "http://user:secret@localhost",
        "http://localhost/path",
        "http://localhost?secret=x",
        "http://localhost/#x",
        "/tmp/bad\n",
    ] {
        assert!(validate_socket(value).is_err(), "{value}");
    }
}

struct Mock {
    list: Value,
    delay: Duration,
    redirect: bool,
    oversize: bool,
    requests: Mutex<Vec<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl Mock {
    fn new(list: Value) -> Self {
        Self {
            list,
            delay: Duration::ZERO,
            redirect: false,
            oversize: false,
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }
    async fn serve(&self, mut socket: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin) {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1024];
        while !bytes.windows(4).any(|s| s == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.unwrap_or(0);
            if n == 0 || bytes.len() > 8192 {
                return;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .unwrap()
            .to_owned();
        self.requests.lock().unwrap().push(request.clone());
        let is_list = request.starts_with("GET /containers/json?all=1 ");
        assert!(
            is_list
                || request.starts_with("GET /containers/")
                    && request.contains("/stats?stream=false&one-shot=true "),
            "{request}"
        );
        if self.redirect {
            let _=socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /control\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return;
        }
        if self.oversize {
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_BODY + 1
                    )
                    .as_bytes(),
                )
                .await;
            return;
        }
        if !is_list {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        let body = if is_list {
            self.list.to_string()
        } else {
            stats().to_string()
        };
        let head=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nLibpod-API-Version: 5.0.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
    }
}

struct Server {
    endpoint: Endpoint,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn start_server(mock: Arc<Mock>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                connection=listener.accept()=>{let (socket,_)=connection.unwrap();let mock=mock.clone();tasks.spawn(async move {mock.serve(socket).await;});},
                result=tasks.join_next(), if !tasks.is_empty()=>{result.unwrap().unwrap();}
            }
        }
    });
    Server { endpoint, task }
}

#[tokio::test]
async fn http_collection_limits_concurrency_and_only_requests_running_stats() {
    let mut rows: Vec<_> = (0..12).map(|i| row(&format!("id{i}"))).collect();
    rows[0]["State"] = json!("exited");
    let mut mock = Mock::new(json!(rows));
    mock.delay = Duration::from_millis(20);
    let mock = Arc::new(mock);
    let server = start_server(mock.clone()).await;
    let rows = Engine::new(&server.endpoint)
        .unwrap()
        .sample(Instant::now() + TOTAL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(rows.len(), 12);
    assert_eq!(rows[0].cpu_pct, None);
    assert_eq!(rows[1].cpu_pct, Some(40.0));
    assert_eq!(rows[1].runtime, "podman");
    assert_eq!(mock.requests.lock().unwrap().len(), 12);
    assert!(mock.peak.load(Ordering::SeqCst) <= 4);
    assert!(mock.peak.load(Ordering::SeqCst) > 1);
}

#[tokio::test]
async fn total_timeout_retains_states_and_redirects_or_large_bodies_fail() {
    let mut mock = Mock::new(json!([row("id")]));
    mock.delay = Duration::from_secs(2);
    let mock = Arc::new(mock);
    let server = start_server(mock.clone()).await;
    let start = Instant::now();
    let rows = Engine::new(&server.endpoint)
        .unwrap()
        .sample(start + Duration::from_millis(80))
        .await
        .unwrap();
    assert_eq!(rows[0].state, "running");
    assert_eq!(rows[0].cpu_pct, None);
    assert!(start.elapsed() < Duration::from_millis(500));
    drop(server);
    for large in [false, true] {
        let mut mock = Mock::new(json!([]));
        mock.oversize = large;
        mock.redirect = !large;
        let mock = Arc::new(mock);
        let server = start_server(mock.clone()).await;
        assert!(Engine::new(&server.endpoint)
            .unwrap()
            .sample(Instant::now() + TOTAL_TIMEOUT)
            .await
            .is_err());
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_uses_the_same_read_only_protocol() {
    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random).unwrap();
    let path = PathBuf::from("/tmp").join(format!("pa-{}.sock", hex::encode(random)));
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        Mock::new(json!([])).serve(socket).await;
    });
    let result = Engine::new(&Endpoint::Unix(path.clone()))
        .unwrap()
        .sample(Instant::now() + TOTAL_TIMEOUT)
        .await;
    task.await.unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn background_collection_reports_and_stops_with_its_owner() {
    let mock = Arc::new(Mock::new(json!([row("id")])));
    let server = start_server(mock).await;
    let Endpoint::Http(url) = &server.endpoint else {
        unreachable!()
    };
    let (tx, mut rx) = watch::channel(Reading::new());
    let task = tokio::spawn(run(Some(url.to_string()), tx));
    tokio::time::timeout(TOTAL_TIMEOUT, rx.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rx.borrow()
            .current_with_max_age(Duration::from_secs(45))
            .unwrap()[0]
            .name,
        "app"
    );
    drop(rx);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}
