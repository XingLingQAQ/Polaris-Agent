use polaris_agent::{
    config::Config,
    retry::{Rejection, Retry},
    rpc::{AgentClient, HttpTransport},
    RpcRequest,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(endpoint: String) -> Config {
    Config {
        endpoint,
        token: "fixture".into(),
        sample_interval_secs: 5,
        report_interval_secs: 60,
        report_interval_explicit: true,
        insecure: false,
        include_nics: vec![],
        exclude_nics: vec![],
        include_mounts: vec![],
        exclude_mounts: vec![],
        auto_update: false,
        log_file: None,
        collect_gpus: false,
        collect_sensors: false,
        collect_containers: false,
        container_socket: None,
        service_tier: None,
        config_path: "unused".into(),
        runtime_overrides: Default::default(),
    }
}

#[tokio::test]
async fn websocket_handshake_preserves_status_and_cooldown_for_the_supervisor() {
    for status in [401, 403, 429, 503] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = config(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).await.unwrap();
            stream.write_all(format!("HTTP/1.1 {status} Rejected\r\nRetry-After: 120\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        });
        let error = AgentClient::connect(&cfg).await.err().unwrap();
        let rejection = error.downcast_ref::<Rejection>().unwrap();
        assert_eq!(rejection.status, status);
        assert_eq!(rejection.retry_after, Some(Duration::from_secs(120)));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_outage_retries_until_acknowledged_and_resets_delay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let requests = attempts.clone();
    let server = tokio::spawn(async move {
        for _ in 0..8 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).await.unwrap();
            let index = requests.fetch_add(1, Ordering::SeqCst);
            let response = if index < 7 {
                "HTTP/1.1 503 Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                let body =
                    r#"{"ok":true,"applied":1,"acks":[{"id":null,"ack":true,"ignored":false}]}"#;
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            };
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let cfg = Config {
        endpoint: format!("http://{address}"),
        token: "fixture".into(),
        sample_interval_secs: 5,
        report_interval_secs: 60,
        report_interval_explicit: true,
        insecure: false,
        include_nics: vec![],
        exclude_nics: vec![],
        include_mounts: vec![],
        exclude_mounts: vec![],
        auto_update: false,
        log_file: None,
        collect_gpus: false,
        collect_sensors: false,
        collect_containers: false,
        container_socket: None,
        service_tier: None,
        config_path: "unused".into(),
        runtime_overrides: Default::default(),
    };
    let client = HttpTransport::new(&cfg).unwrap();
    let frame = RpcRequest::notification("report", serde_json::json!({}));
    let mut retry = Retry::new(Duration::from_millis(5), Duration::from_millis(60));
    for index in 0..8 {
        match client.send_status(&frame).await {
            Ok(true) => {
                assert_eq!(index, 7);
                retry.success("http reports");
            }
            Err(error) => {
                assert_eq!(error.downcast_ref::<Rejection>().unwrap().status, 503);
                let pause = retry.fail(&error, "http reports");
                tokio::time::sleep(pause).await;
            }
            result => panic!("unexpected acknowledgement: {result:?}"),
        }
    }
    assert_eq!(retry.failures(), 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 8);
    server.await.unwrap();
}

#[cfg(unix)]
#[test]
fn stop_during_unresponsive_startup_is_bounded_and_logged() {
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = std::env::temp_dir().join(format!(
        "polaris-stalled-startup-{}.json",
        std::process::id()
    ));
    std::fs::write(&config, "{}").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_polaris-agent"))
        .args([
            "run",
            "--endpoint",
            &format!("http://{}", listener.local_addr().unwrap()),
            "--token",
            "fixture",
        ])
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .env("NO_PROXY", "*")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(8) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let exited = child.try_wait().unwrap().is_some();
    if !exited {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    std::fs::remove_file(config).unwrap();
    assert!(exited, "Agent did not stop during a stalled request");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("starting polaris-agent"));
}
