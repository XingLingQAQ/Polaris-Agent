use super::{
    bounded_number, hardware_name, GpuStats, Reading, POLL_INTERVAL, READ_TIMEOUT, RETRY_INTERVAL,
};
use std::{io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, sync::watch, time::timeout};

const MAX_OUTPUT: usize = 32 * 1024;
const MAX_GPUS: usize = 16;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) async fn run(tx: watch::Sender<Reading<GpuStats>>) {
    let Ok(Some(executable)) = tokio::task::spawn_blocking(find_executable).await else {
        return;
    };
    loop {
        let mut command = Command::new(&executable);
        command.args([
            "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ]);
        let result = match bounded_output(&mut command, READ_TIMEOUT).await {
            Ok(bytes) => parse(&bytes),
            Err(error) => Err(error),
        };
        let delay = match result {
            Ok(gpus) => {
                let never_present = gpus.is_empty() && tx.borrow().values.is_none();
                tx.send_modify(|reading| reading.update(gpus));
                if never_present {
                    return;
                }
                POLL_INTERVAL
            }
            Err(error) => {
                tracing::debug!(%error, "GPU telemetry unavailable; retrying in 60 seconds");
                tx.send_modify(|reading| reading.update(Vec::new()));
                RETRY_INTERVAL
            }
        };
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn find_executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            candidates.push(PathBuf::from(root).join("System32/nvidia-smi.exe"));
        }
        for key in ["ProgramW6432", "ProgramFiles"] {
            if let Some(root) = std::env::var_os(key) {
                candidates
                    .push(PathBuf::from(root).join("NVIDIA Corporation/NVSMI/nvidia-smi.exe"));
            }
        }
    }
    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/bin/nvidia-smi"),
        PathBuf::from("/usr/local/bin/nvidia-smi"),
    ]);
    let filename = if cfg!(target_os = "windows") {
        "nvidia-smi.exe"
    } else {
        "nvidia-smi"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&paths)
                .take(128)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(filename)),
        );
    }
    candidates
        .into_iter()
        .find(|p| p.is_absolute() && p.is_file())
}

async fn bounded_output(command: &mut Command, deadline: Duration) -> io::Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000);
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout")
        .take((MAX_OUTPUT + 1) as u64);
    let result = timeout(deadline, async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        if bytes.len() > MAX_OUTPUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "GPU output exceeds 32 KiB",
            ));
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(io::Error::other(format!("nvidia-smi exited with {status}")));
        }
        Ok(bytes)
    })
    .await
    .unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "GPU collection timed out",
        ))
    });
    if result.is_err() {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(1), child.wait()).await;
    }
    result
}

fn mib_to_bytes(text: &str) -> Option<u64> {
    let bytes = (bounded_number(text, 0.0, MAX_SAFE_INTEGER as f64)? * 1_048_576.0).round() as u64;
    (bytes <= MAX_SAFE_INTEGER).then_some(bytes)
}

fn parse(bytes: &[u8]) -> io::Result<Vec<GpuStats>> {
    if bytes.len() > MAX_OUTPUT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    let mut gpus = Vec::<GpuStats>::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Some((index, rest)) = line.split_once(',') else {
            continue;
        };
        let Ok(index) = index.trim().parse::<u16>() else {
            continue;
        };
        if gpus.iter().any(|gpu| gpu.index == index) {
            continue;
        }
        // Names can contain commas; the final five columns are numeric.
        let fields: Vec<_> = rest.rsplitn(6, ',').map(str::trim).collect();
        if fields.len() != 6 {
            continue;
        }
        let Some(name) = hardware_name(&fields[5].trim_matches('"').replace("\"\"", "\"")) else {
            continue;
        };
        let mut used = mib_to_bytes(fields[3]);
        let total = mib_to_bytes(fields[2]);
        if matches!((used, total), (Some(u), Some(t)) if u > t) {
            used = None;
        }
        gpus.push(GpuStats {
            index,
            name,
            vendor: "NVIDIA".into(),
            util_pct: bounded_number(fields[4], 0.0, 100.0),
            mem_used_bytes: used,
            mem_total_bytes: total,
            temp_c: bounded_number(fields[1], -100.0, 250.0),
            power_w: bounded_number(fields[0], 0.0, 10_000.0),
        });
        if gpus.len() == MAX_GPUS {
            break;
        }
    }
    if gpus.is_empty() && !text.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no valid GPU rows",
        ));
    }
    Ok(gpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_preserves_units_unknown_metrics_and_names_with_commas() {
        let gpus = parse(b"0, NVIDIA RTX 4090, 64.375, 1024, 24576, 64.375, 125.44\r\n1, \"GPU, secondary\", [N/A], N/A, 8192, [Not Supported], N/A\n").unwrap();
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].mem_used_bytes, Some(1_073_741_824));
        assert_eq!(gpus[0].mem_total_bytes, Some(25_769_803_776));
        assert_eq!(gpus[0].temp_c, Some(64.375));
        assert_eq!(gpus[0].power_w, Some(125.44));
        assert_eq!(gpus[1].name, "GPU, secondary");
        assert_eq!(gpus[1].util_pct, None);
        assert_eq!(gpus[1].mem_used_bytes, None);
        assert_eq!(gpus[1].temp_c, None);
        assert_eq!(gpus[1].power_w, None);
    }

    #[test]
    fn fractional_mib_rounds_to_bytes_within_the_safe_integer_limit() {
        assert_eq!(mib_to_bytes("1.1"), Some(1_153_434));
        assert_eq!(mib_to_bytes("1.01"), Some(1_059_062));
        assert_eq!(mib_to_bytes("0.000000476837158203125"), Some(1));
        assert_eq!(mib_to_bytes("0"), Some(0));
        assert_eq!(mib_to_bytes("1024"), Some(1_073_741_824));
        let largest_mib = MAX_SAFE_INTEGER as f64 / 1_048_576.0;
        assert_eq!(
            mib_to_bytes(&largest_mib.to_string()),
            Some(MAX_SAFE_INTEGER)
        );
        for value in [
            "8589934592",
            "9007199254740991",
            "1e300",
            "-0.01",
            "NaN",
            "inf",
            "N/A",
        ] {
            assert_eq!(mib_to_bytes(value), None, "{value}");
        }
        let gpus = parse(b"0, GPU, 50, 1.1, 1.01, 60, 100").unwrap();
        assert_eq!(gpus[0].mem_used_bytes, None);
        assert_eq!(gpus[0].mem_total_bytes, Some(1_059_062));
    }

    #[test]
    fn rejects_unsafe_numbers_duplicates_and_invalid_identities() {
        let gpus = parse(b"-1, GPU, 0, 0, 0, 0, 0\n65536, GPU, 0, 0, 0, 0, 0\n0, GPU, NaN, 1025, 1024, inf, 10001\n0, duplicate, 0, 0, 0, 0, 0\n1, GPU, 101, 9007199254740991, -1, -101, -1\n").unwrap();
        assert_eq!(gpus.len(), 2);
        for gpu in gpus {
            assert_eq!(gpu.util_pct, None);
            assert_eq!(gpu.mem_used_bytes, None);
            assert_eq!(gpu.temp_c, None);
            assert_eq!(gpu.power_w, None);
        }
        assert!(parse(b"not csv").is_err());
        assert!(parse(b"0, , 0, 0, 0, 0, 0").is_err());
        assert!(parse(&[0xff]).is_err());
        assert!(parse(&vec![b' '; MAX_OUTPUT + 1]).is_err());
        assert!(parse(b"\r\n").unwrap().is_empty());
    }

    #[test]
    fn output_is_capped_and_wire_fields_match_the_server_contract() {
        let text = (0..30)
            .map(|i| format!("{i}, {}, 100, 0, 0, 250, 10000\n", "显".repeat(200)))
            .collect::<String>();
        let gpus = parse(text.as_bytes()).unwrap();
        assert_eq!(gpus.len(), MAX_GPUS);
        assert!(gpus.iter().all(|gpu| gpu.name.chars().count() == 128));
        let snapshot = crate::sysstat::SysSnapshot {
            gpus: Some(gpus),
            ..Default::default()
        };
        let report =
            serde_json::to_value(crate::rpc::protocol::build_report(&snapshot, 1, &[])).unwrap();
        assert_eq!(report["params"]["gpus"][0]["vendor"], "NVIDIA");
        assert_eq!(report["params"]["gpus"][0]["mem_used_bytes"], 0);
        assert!(report["params"].get("sensors").is_none());
    }

    #[test]
    fn command_fixture() {
        match std::env::var("POLARIS_HARDWARE_TEST").as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            Ok("large") => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&vec![b'x'; MAX_OUTPUT * 4]);
            }
            Ok("fail") => std::process::exit(9),
            _ => {}
        }
    }

    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "sysstat::hardware::nvidia::tests::command_fixture",
                "--nocapture",
            ])
            .env("POLARIS_HARDWARE_TEST", mode);
        command
    }

    #[tokio::test]
    async fn subprocess_is_bounded_and_does_not_block_the_runtime() {
        let start = std::time::Instant::now();
        let task = tokio::spawn(async {
            bounded_output(&mut fixture_command("sleep"), Duration::from_millis(150)).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(
            bounded_output(&mut fixture_command("large"), READ_TIMEOUT)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(bounded_output(&mut fixture_command("fail"), READ_TIMEOUT)
            .await
            .is_err());
        assert!(bounded_output(&mut fixture_command("ok"), READ_TIMEOUT)
            .await
            .is_ok());
        assert_eq!(
            bounded_output(
                &mut Command::new("polaris-nonexistent-gpu-collector"),
                READ_TIMEOUT
            )
            .await
            .unwrap_err()
            .kind(),
            io::ErrorKind::NotFound
        );
    }
}
