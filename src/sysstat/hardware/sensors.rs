use super::{bounded_number, hardware_name, SensorKind, SensorStats};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_SENSORS: usize = 64;

#[derive(Clone)]
struct SensorSource {
    input: PathBuf,
    name: String,
    kind: SensorKind,
}

pub(super) async fn run(
    root: PathBuf,
    tx: tokio::sync::watch::Sender<super::Reading<SensorStats>>,
) {
    use super::{POLL_INTERVAL, READ_TIMEOUT, RETRY_INTERVAL};
    let discovery = tokio::task::spawn_blocking(move || discover(&root));
    let Ok(Ok(Ok(sources))) = tokio::time::timeout(READ_TIMEOUT, discovery).await else {
        return;
    };
    if sources.is_empty() {
        return;
    }
    loop {
        let inputs = sources.clone();
        let mut task = tokio::task::spawn_blocking(move || collect(&inputs));
        let delay = match tokio::time::timeout(READ_TIMEOUT, &mut task).await {
            Ok(Ok(sensors)) => {
                let failed = sensors.is_empty();
                tx.send_modify(|reading| reading.update(sensors));
                if failed {
                    RETRY_INTERVAL
                } else {
                    POLL_INTERVAL
                }
            }
            result => {
                tx.send_modify(|reading| reading.update(Vec::new()));
                tracing::debug!("temperature telemetry unavailable; retrying in 60 seconds");
                // A stuck sysfs read must not accumulate blocking tasks.
                if result.is_err() {
                    let _ = task.await;
                }
                RETRY_INTERVAL
            }
        };
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn read_small(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    fs::File::open(path)?.take(1025).read_to_string(&mut text)?;
    if text.len() > 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(text)
}

fn discover(root: &Path) -> io::Result<Vec<SensorSource>> {
    let mut chips: Vec<_> = fs::read_dir(root)?
        .take(128)
        .filter_map(Result::ok)
        .filter(|entry| numbered(&entry.file_name().to_string_lossy(), "hwmon", ""))
        .collect();
    chips.sort_by_key(|entry| entry.file_name());
    let mut sources = Vec::new();
    for chip in chips {
        let dir = chip.path();
        let id = chip.file_name().to_string_lossy().into_owned();
        let name = read_small(&dir.join("name"))
            .ok()
            .and_then(|s| hardware_name(&s))
            .unwrap_or_else(|| id.clone());
        let kind = classify(&name);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut inputs: Vec<_> = entries
            .take(1024)
            .filter_map(Result::ok)
            .filter(|entry| numbered(&entry.file_name().to_string_lossy(), "temp", "_input"))
            .collect();
        inputs.sort_by_key(|entry| entry.file_name());
        for input in inputs {
            let filename = input.file_name().to_string_lossy().into_owned();
            let stem = filename.trim_end_matches("_input");
            let label = read_small(&dir.join(format!("{stem}_label")))
                .ok()
                .and_then(|s| hardware_name(&s))
                .unwrap_or_else(|| stem.to_owned());
            let display = hardware_name(&format!("{id}/{stem} {name}: {label}"))
                .expect("nonempty sensor identity");
            sources.push(SensorSource {
                input: input.path(),
                name: display,
                kind,
            });
            if sources.len() == MAX_SENSORS {
                return Ok(sources);
            }
        }
    }
    Ok(sources)
}

fn numbered(text: &str, prefix: &str, suffix: &str) -> bool {
    text.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(suffix))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
}

fn classify(chip: &str) -> SensorKind {
    let chip = chip.to_ascii_lowercase();
    if [
        "coretemp",
        "k10temp",
        "k8temp",
        "zenpower",
        "cpu_thermal",
        "cpu-thermal",
    ]
    .iter()
    .any(|s| chip == *s)
    {
        SensorKind::Cpu
    } else if ["nvme", "drivetemp"].iter().any(|s| chip == *s) {
        SensorKind::Disk
    } else if ["amdgpu", "radeon", "nouveau", "nvidia", "i915", "xe"]
        .iter()
        .any(|s| chip == *s)
    {
        SensorKind::Gpu
    } else {
        SensorKind::Other
    }
}

fn optional_flag(path: &Path, expected: &str) -> bool {
    match read_small(path) {
        Ok(value) => value.trim() == expected,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

fn collect(sources: &[SensorSource]) -> Vec<SensorStats> {
    sources
        .iter()
        .filter_map(|source| {
            let stem = source.input.file_name()?.to_str()?.strip_suffix("_input")?;
            if !optional_flag(&source.input.with_file_name(format!("{stem}_fault")), "0")
                || !optional_flag(&source.input.with_file_name(format!("{stem}_enable")), "1")
            {
                return None;
            }
            let text = read_small(&source.input).ok()?;
            let temp_c = bounded_number(&text, -100_000.0, 250_000.0)? / 1000.0;
            Some(SensorStats {
                name: source.name.clone(),
                kind: source.kind,
                temp_c,
            })
        })
        .take(MAX_SENSORS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut random = [0u8; 16];
            getrandom::getrandom(&mut random).unwrap();
            let root = std::env::temp_dir().join(format!("polaris-hwmon-{}", hex::encode(random)));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, value: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn background_collection_reaches_snapshots_and_stops_when_unused() {
        use super::super::{HardwareReader, Reading, READ_TIMEOUT};
        let f = Fixture::new();
        f.write("hwmon0/name", "coretemp");
        f.write("hwmon0/temp1_input", "64375");
        let (tx, mut rx) = tokio::sync::watch::channel(Reading::new());
        let task = tokio::spawn(run(f.0.clone(), tx));
        tokio::time::timeout(READ_TIMEOUT, rx.changed())
            .await
            .unwrap()
            .unwrap();
        let reader = HardwareReader {
            gpus: None,
            sensors: Some(rx),
        };
        let mut snapshot = crate::sysstat::SysSnapshot::default();
        reader.apply(&mut snapshot);
        assert_eq!(snapshot.sensors.unwrap()[0].temp_c, 64.375);
        assert!(snapshot.gpus.is_none());
        drop(reader);
        tokio::time::timeout(READ_TIMEOUT, task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn absent_hardware_finishes_discovery_without_polling_or_empty_fields() {
        use super::super::{Reading, READ_TIMEOUT};
        let f = Fixture::new();
        for root in [f.0.clone(), f.0.join("absent")] {
            let (tx, rx) = tokio::sync::watch::channel(Reading::new());
            tokio::time::timeout(READ_TIMEOUT, run(root, tx))
                .await
                .unwrap();
            assert!(rx.borrow().current().is_none());
            assert!(rx.has_changed().is_err());
        }
    }

    #[test]
    fn hwmon_discovers_names_kinds_and_millidegrees_without_extra_files() {
        let f = Fixture::new();
        for (id, chip, label, value) in [
            (0, "coretemp", "Package id 0", "64375\n"),
            (1, "nvme", "Composite", "85000"),
            (2, "amdgpu", "edge", "95000"),
            (3, "acpitz", "", "-100000"),
        ] {
            f.write(&format!("hwmon{id}/name"), chip);
            f.write(&format!("hwmon{id}/temp1_label"), label);
            f.write(&format!("hwmon{id}/temp1_input"), value);
            f.write(&format!("hwmon{id}/fan1_input"), "1234");
        }
        let sources = discover(&f.0).unwrap();
        let sensors = collect(&sources);
        assert_eq!(sensors.len(), 4);
        assert_eq!(sensors[0].name, "hwmon0/temp1 coretemp: Package id 0");
        assert_eq!(sensors[0].temp_c, 64.375);
        assert_eq!(sensors[0].kind, SensorKind::Cpu);
        assert_eq!(sensors[1].kind, SensorKind::Disk);
        assert_eq!(sensors[2].kind, SensorKind::Gpu);
        assert_eq!(sensors[3].kind, SensorKind::Other);
        assert!(sensors[3].name.ends_with("temp1"));
        assert_eq!(sensors[3].temp_c, -100.0);
        let snapshot = crate::sysstat::SysSnapshot {
            sensors: Some(sensors),
            ..Default::default()
        };
        let report =
            serde_json::to_value(crate::rpc::protocol::build_report(&snapshot, 1, &[])).unwrap();
        assert_eq!(report["params"]["sensors"][0]["kind"], "cpu");
        assert_eq!(report["params"]["sensors"][0]["temp_c"], 64.375);
        assert!(report["params"].get("gpus").is_none());
    }

    #[test]
    fn missing_disabled_faulty_and_out_of_range_sensors_are_skipped() {
        let f = Fixture::new();
        assert!(discover(&f.0).unwrap().is_empty());
        for (index, value) in [
            "NaN", "inf", "250001", "-100001", "no", "85000", "95000", "250000",
        ]
        .iter()
        .enumerate()
        {
            f.write(&format!("hwmon0/temp{index}_input"), value);
        }
        f.write("hwmon0/temp5_fault", "1");
        f.write("hwmon0/temp6_enable", "0");
        let sources = discover(&f.0).unwrap();
        let sensors = collect(&sources);
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].temp_c, 250.0);
        fs::remove_file(f.0.join("hwmon0/temp7_input")).unwrap();
        assert!(collect(&sources).is_empty());
    }

    #[test]
    fn discovery_bounds_count_names_and_input_size() {
        let f = Fixture::new();
        for i in 0..80 {
            f.write(&format!("hwmon0/temp{i}_input"), "25000");
            f.write(&format!("hwmon0/temp{i}_label"), &"温".repeat(200));
        }
        f.write("hwmon0/tempbad_input", "95000");
        let sources = discover(&f.0).unwrap();
        assert_eq!(sources.len(), 64);
        let sensors = collect(&sources);
        assert_eq!(sensors.len(), 64);
        assert!(sensors.iter().all(|s| s.name.chars().count() == 128));
        fs::write(&sources[0].input, "0".repeat(1025)).unwrap();
        assert_eq!(collect(&sources).len(), 63);
    }
}
