mod nvidia;
#[cfg(any(target_os = "linux", test))]
mod sensors;

use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::{sync::watch, task::JoinHandle};

const POLL_INTERVAL: Duration = Duration::from_secs(10);
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GpuStats {
    pub index: u16,
    pub name: String,
    pub vendor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    Cpu,
    Disk,
    Gpu,
    Other,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SensorStats {
    pub name: String,
    pub kind: SensorKind,
    pub temp_c: f64,
}

struct Reading<T> {
    values: Option<Vec<T>>,
    at: Instant,
}

impl<T: Clone> Reading<T> {
    fn new() -> Self {
        Self {
            values: None,
            at: Instant::now(),
        }
    }

    fn update(&mut self, values: Vec<T>) {
        // None omits the field; [] clears readings already stored by the server.
        if self.values.is_some() || !values.is_empty() {
            self.values = Some(values);
        }
        self.at = Instant::now();
    }

    fn current(&self) -> Option<Vec<T>> {
        self.values.as_ref().map(|values| {
            if self.at.elapsed() > MAX_AGE {
                Vec::new()
            } else {
                values.clone()
            }
        })
    }
}

#[derive(Clone, Default)]
pub struct HardwareReader {
    gpus: Option<watch::Receiver<Reading<GpuStats>>>,
    sensors: Option<watch::Receiver<Reading<SensorStats>>>,
}

impl HardwareReader {
    pub fn apply(&self, snapshot: &mut super::SysSnapshot) {
        snapshot.gpus = self.gpus.as_ref().and_then(|rx| rx.borrow().current());
        snapshot.sensors = self.sensors.as_ref().and_then(|rx| rx.borrow().current());
    }
}

pub struct HardwareTelemetry {
    reader: HardwareReader,
    tasks: Vec<JoinHandle<()>>,
}

impl HardwareTelemetry {
    pub fn start(collect_gpus: bool, collect_sensors: bool) -> Self {
        let mut telemetry = Self {
            reader: HardwareReader::default(),
            tasks: Vec::new(),
        };
        if collect_gpus && cfg!(any(target_os = "linux", target_os = "windows")) {
            let (tx, rx) = watch::channel(Reading::new());
            telemetry.reader.gpus = Some(rx);
            telemetry.tasks.push(tokio::spawn(nvidia::run(tx)));
        }
        #[cfg(target_os = "linux")]
        if collect_sensors {
            let (tx, rx) = watch::channel(Reading::new());
            telemetry.reader.sensors = Some(rx);
            telemetry.tasks.push(tokio::spawn(sensors::run(
                std::path::PathBuf::from("/sys/class/hwmon"),
                tx,
            )));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = collect_sensors;
        telemetry
    }

    pub fn reader(&self) -> HardwareReader {
        self.reader.clone()
    }
}

impl Drop for HardwareTelemetry {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn bounded_number(text: &str, min: f64, max: f64) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= min && *v <= max)
}

fn hardware_name(text: &str) -> Option<String> {
    let name: String = text
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_expired_and_cleared_readings_have_distinct_wire_meanings() {
        let mut reading = Reading::<u8>::new();
        reading.update(vec![]);
        assert_eq!(reading.current(), None);
        reading.update(vec![7]);
        assert_eq!(reading.current(), Some(vec![7]));
        reading.at = Instant::now() - MAX_AGE - Duration::from_secs(1);
        assert_eq!(reading.current(), Some(vec![]));
        reading.update(vec![9]);
        assert_eq!(reading.current(), Some(vec![9]));
        reading.update(vec![]);
        assert_eq!(reading.current(), Some(vec![]));
    }

    #[test]
    fn disabled_collectors_start_no_tasks_and_emit_no_fields() {
        let telemetry = HardwareTelemetry::start(false, false);
        assert!(telemetry.tasks.is_empty());
        let mut snapshot = super::super::SysSnapshot::default();
        telemetry.reader().apply(&mut snapshot);
        let report = crate::rpc::protocol::build_report(&snapshot, 1, &[]);
        let json = serde_json::to_value(report).unwrap();
        assert!(json["params"].get("gpus").is_none());
        assert!(json["params"].get("sensors").is_none());
    }

    #[test]
    fn names_are_bounded_unicode_without_control_characters() {
        assert_eq!(hardware_name("\0 \n"), None);
        assert_eq!(hardware_name(" GPU\u{7f}\n "), Some("GPU".into()));
        assert_eq!(
            hardware_name(&"显卡".repeat(100)).unwrap().chars().count(),
            128
        );
    }
}
