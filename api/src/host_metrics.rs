use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct HostMetricsService {
    inner: Arc<Mutex<HostMetricsCollector>>,
}

impl HostMetricsService {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HostMetricsCollector::new(data_dir.as_ref()))),
        }
    }

    pub async fn snapshot(&self) -> HostMetricsSnapshot {
        self.inner.lock().await.snapshot()
    }
}

struct HostMetricsCollector {
    data_dir: PathBuf,
    previous_cpu: Option<CpuSample>,
    previous_disk: Option<CounterSample>,
    previous_network: Option<CounterSample>,
}

impl HostMetricsCollector {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            previous_cpu: read_cpu_sample(),
            previous_disk: read_disk_io_sample(),
            previous_network: read_network_sample(),
        }
    }

    fn snapshot(&mut self) -> HostMetricsSnapshot {
        let cpu = cpu_metric(&mut self.previous_cpu);
        let memory = memory_metric();
        let disk = disk_usage_metric(&self.data_dir);
        let disk_rate = rate_metric(
            &mut self.previous_disk,
            read_disk_io_sample(),
            "读",
            "写",
            "磁盘速率暂不可用",
        );
        let network_rate = rate_metric(
            &mut self.previous_network,
            read_network_sample(),
            "入",
            "出",
            "网络速率暂不可用",
        );

        HostMetricsSnapshot {
            cpu,
            memory,
            disk,
            disk_rate,
            network_rate,
            sampled_at_epoch_ms: unix_epoch_millis(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HostMetricsSnapshot {
    pub cpu: PercentMetric,
    pub memory: UsageMetric,
    pub disk: DiskUsageMetric,
    pub disk_rate: RateMetric,
    pub network_rate: RateMetric,
    pub sampled_at_epoch_ms: u128,
}

#[derive(Debug, Serialize)]
pub struct PercentMetric {
    pub percent: f64,
    pub percent_label: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct UsageMetric {
    pub percent: f64,
    pub percent_label: String,
    pub used_label: String,
    pub total_label: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct DiskUsageMetric {
    pub percent: f64,
    pub percent_label: String,
    pub used_label: String,
    pub total_label: String,
    pub detail: String,
    pub mount_point: String,
}

#[derive(Debug, Serialize)]
pub struct RateMetric {
    pub read_label: String,
    pub write_label: String,
    pub detail: String,
    pub utilization_percent: Option<f64>,
    pub utilization_label: String,
}

#[derive(Clone, Copy)]
struct CpuSample {
    idle: u64,
    total: u64,
}

#[derive(Clone)]
struct CounterSample {
    read: u64,
    write: u64,
    busy_millis_by_device: HashMap<String, u64>,
    sampled_at: Instant,
}

fn cpu_metric(previous: &mut Option<CpuSample>) -> PercentMetric {
    let Some(current) = read_cpu_sample() else {
        return PercentMetric {
            percent: 0.0,
            percent_label: "--".to_owned(),
            detail: unsupported_detail(),
        };
    };

    let percent = previous
        .map(|old| {
            let idle_delta = current.idle.saturating_sub(old.idle);
            let total_delta = current.total.saturating_sub(old.total);
            if total_delta == 0 {
                0.0
            } else {
                ((total_delta.saturating_sub(idle_delta)) as f64 / total_delta as f64) * 100.0
            }
        })
        .unwrap_or_default();
    *previous = Some(current);

    let percent = truncated_percent(percent);
    PercentMetric {
        percent,
        percent_label: percent_label(percent),
        detail: cpu_detail(),
    }
}

fn memory_metric() -> UsageMetric {
    let Some((total, available)) = read_memory_sample() else {
        return UsageMetric {
            percent: 0.0,
            percent_label: "--".to_owned(),
            used_label: "--".to_owned(),
            total_label: "--".to_owned(),
            detail: unsupported_detail(),
        };
    };

    let used = total.saturating_sub(available);
    usage_view(
        used,
        total,
        format!(
            "{} / {}",
            bytes_label(used as f64),
            bytes_label(total as f64)
        ),
    )
}

fn disk_usage_metric(data_dir: &Path) -> DiskUsageMetric {
    let Some(sample) = read_disk_usage_sample(data_dir) else {
        return DiskUsageMetric {
            percent: 0.0,
            percent_label: "--".to_owned(),
            used_label: "--".to_owned(),
            total_label: "--".to_owned(),
            detail: "磁盘容量暂不可用".to_owned(),
            mount_point: "--".to_owned(),
        };
    };

    let percent = usage_percent(sample.used, sample.total);
    DiskUsageMetric {
        percent,
        percent_label: percent_label(percent),
        used_label: bytes_label(sample.used as f64),
        total_label: bytes_label(sample.total as f64),
        detail: format!(
            "{} / {}",
            bytes_label(sample.used as f64),
            bytes_label(sample.total as f64)
        ),
        mount_point: sample.mount_point,
    }
}

fn usage_view(used: u64, total: u64, detail: String) -> UsageMetric {
    let percent = usage_percent(used, total);
    UsageMetric {
        percent,
        percent_label: percent_label(percent),
        used_label: bytes_label(used as f64),
        total_label: bytes_label(total as f64),
        detail,
    }
}

fn rate_metric(
    previous: &mut Option<CounterSample>,
    current: Option<CounterSample>,
    read_label_prefix: &str,
    write_label_prefix: &str,
    unavailable_detail: &str,
) -> RateMetric {
    let Some(current) = current else {
        return RateMetric {
            read_label: "--".to_owned(),
            write_label: "--".to_owned(),
            detail: unavailable_detail.to_owned(),
            utilization_percent: None,
            utilization_label: "--".to_owned(),
        };
    };

    let (read_rate, write_rate, utilization_percent) = previous
        .as_ref()
        .map(|old| {
            let elapsed = current
                .sampled_at
                .duration_since(old.sampled_at)
                .as_secs_f64()
                .max(0.001);
            (
                current.read.saturating_sub(old.read) as f64 / elapsed,
                current.write.saturating_sub(old.write) as f64 / elapsed,
                disk_busy_percent(old, &current, elapsed),
            )
        })
        .unwrap_or_default();
    *previous = Some(current);

    let read_label = rate_label(read_rate);
    let write_label = rate_label(write_rate);
    let utilization_label = utilization_percent
        .map(percent_label)
        .unwrap_or_else(|| "--".to_owned());
    RateMetric {
        read_label: read_label.clone(),
        write_label: write_label.clone(),
        detail: format!("{read_label_prefix} {read_label} · {write_label_prefix} {write_label}"),
        utilization_percent,
        utilization_label,
    }
}

fn disk_busy_percent(
    old: &CounterSample,
    current: &CounterSample,
    elapsed_secs: f64,
) -> Option<f64> {
    if current.busy_millis_by_device.is_empty() || elapsed_secs <= 0.0 {
        return None;
    }

    let elapsed_millis = elapsed_secs * 1000.0;
    current
        .busy_millis_by_device
        .iter()
        .filter_map(|(device, current_busy)| {
            let old_busy = old.busy_millis_by_device.get(device)?;
            Some(current_busy.saturating_sub(*old_busy) as f64 / elapsed_millis * 100.0)
        })
        .reduce(f64::max)
        .map(truncated_percent)
}

#[cfg(target_os = "linux")]
fn read_cpu_sample() -> Option<CpuSample> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }

    let values = parts
        .filter_map(|part| part.parse::<u64>().ok())
        .collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }
    let idle = values
        .get(3)
        .copied()
        .unwrap_or_default()
        .saturating_add(values.get(4).copied().unwrap_or_default());
    let total = values.iter().copied().sum();
    Some(CpuSample { idle, total })
}

#[cfg(not(target_os = "linux"))]
fn read_cpu_sample() -> Option<CpuSample> {
    None
}

#[cfg(target_os = "linux")]
fn read_memory_sample() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in content.lines() {
        if let Some(value) = parse_meminfo_kib(line, "MemTotal:") {
            total = Some(value);
        } else if let Some(value) = parse_meminfo_kib(line, "MemAvailable:") {
            available = Some(value);
        }
    }
    Some((total?, available?))
}

#[cfg(not(target_os = "linux"))]
fn read_memory_sample() -> Option<(u64, u64)> {
    None
}

#[cfg(target_os = "linux")]
fn parse_meminfo_kib(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?.trim();
    let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
    Some(kib.saturating_mul(1024))
}

#[cfg(target_os = "linux")]
struct DiskUsageSample {
    total: u64,
    used: u64,
    mount_point: String,
}

#[cfg(target_os = "linux")]
fn read_disk_usage_sample(data_dir: &Path) -> Option<DiskUsageSample> {
    let output = std::process::Command::new("df")
        .arg("-B1")
        .arg("--output=target,size,used")
        .arg(data_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with("Mounted"))?;
    parse_df_line(line)
}

#[cfg(not(target_os = "linux"))]
fn read_disk_usage_sample(_data_dir: &Path) -> Option<DiskUsageSample> {
    None
}

#[cfg(not(target_os = "linux"))]
struct DiskUsageSample {
    total: u64,
    used: u64,
    mount_point: String,
}

#[cfg(target_os = "linux")]
fn parse_df_line(line: &str) -> Option<DiskUsageSample> {
    let mut parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    let used = parts.pop()?.parse::<u64>().ok()?;
    let total = parts.pop()?.parse::<u64>().ok()?;
    let mount_point = parts.join(" ");
    Some(DiskUsageSample {
        total,
        used,
        mount_point,
    })
}

#[cfg(target_os = "linux")]
fn read_disk_io_sample() -> Option<CounterSample> {
    let block_devices = linux_block_devices();
    let content = std::fs::read_to_string("/proc/diskstats").ok()?;
    let mut read = 0_u64;
    let mut write = 0_u64;
    let mut busy_millis_by_device = HashMap::new();

    for line in content.lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 14 {
            continue;
        }
        let name = parts[2];
        if !block_devices.iter().any(|device| device == name) || is_ignored_block_device(name) {
            continue;
        }
        let sectors_read = parts[5].parse::<u64>().unwrap_or_default();
        let sectors_written = parts[9].parse::<u64>().unwrap_or_default();
        let busy_millis = parts[12].parse::<u64>().unwrap_or_default();
        read = read.saturating_add(sectors_read.saturating_mul(512));
        write = write.saturating_add(sectors_written.saturating_mul(512));
        busy_millis_by_device.insert(name.to_owned(), busy_millis);
    }

    Some(CounterSample {
        read,
        write,
        busy_millis_by_device,
        sampled_at: Instant::now(),
    })
}

#[cfg(not(target_os = "linux"))]
fn read_disk_io_sample() -> Option<CounterSample> {
    None
}

#[cfg(target_os = "linux")]
fn linux_block_devices() -> Vec<String> {
    std::fs::read_dir("/sys/block")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

#[cfg(target_os = "linux")]
fn is_ignored_block_device(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("sr")
        || name.starts_with("fd")
}

#[cfg(target_os = "linux")]
fn read_network_sample() -> Option<CounterSample> {
    let content = std::fs::read_to_string("/proc/net/dev").ok()?;
    let mut read = 0_u64;
    let mut write = 0_u64;

    for line in content.lines().skip(2) {
        let Some((name, stats)) = line.split_once(':') else {
            continue;
        };
        if is_loopback_interface(name.trim()) {
            continue;
        }
        let values = stats.split_whitespace().collect::<Vec<_>>();
        if values.len() < 16 {
            continue;
        }
        read = read.saturating_add(values[0].parse::<u64>().unwrap_or_default());
        write = write.saturating_add(values[8].parse::<u64>().unwrap_or_default());
    }

    Some(CounterSample {
        read,
        write,
        busy_millis_by_device: HashMap::new(),
        sampled_at: Instant::now(),
    })
}

#[cfg(not(target_os = "linux"))]
fn read_network_sample() -> Option<CounterSample> {
    None
}

#[cfg(target_os = "linux")]
fn is_loopback_interface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "lo" || lower.contains("loopback")
}

#[cfg(target_os = "linux")]
fn cpu_detail() -> String {
    std::thread::available_parallelism()
        .map(|cores| format!("{} 核心", cores.get()))
        .unwrap_or_else(|_| "CPU 核心数未知".to_owned())
}

#[cfg(not(target_os = "linux"))]
fn cpu_detail() -> String {
    unsupported_detail()
}

fn usage_percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        truncated_percent((used as f64 / total as f64) * 100.0)
    }
}

fn truncated_percent(value: f64) -> f64 {
    let value = value.clamp(0.0, 100.0);
    truncate_two_decimals(value)
}

fn percent_label(value: f64) -> String {
    format!("{value:.2}%")
}

fn bytes_label(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0_usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", value.floor() as u64, UNITS[unit])
    } else {
        format!("{:.2} {}", truncate_two_decimals(value), UNITS[unit])
    }
}

fn rate_label(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes_label(bytes_per_sec))
}

fn truncate_two_decimals(value: f64) -> f64 {
    (value * 100.0).floor() / 100.0
}

fn unsupported_detail() -> String {
    "当前系统不支持采集".to_owned()
}

fn unix_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn formats_byte_rates() {
        assert_eq!(rate_label(0.0), "0 B/s");
        assert_eq!(rate_label(1024.0), "1.00 KB/s");
        assert_eq!(rate_label(1536.99), "1.50 KB/s");
        assert_eq!(rate_label(1024.0 * 1024.0), "1.00 MB/s");
        assert_eq!(bytes_label(1024.0 * 1024.0 * 1024.0), "1.00 GB");
        assert_eq!(bytes_label(-1.0), "0 B");
    }

    #[test]
    fn clamps_usage_percent() {
        assert_eq!(usage_percent(50, 100), 50.0);
        assert_eq!(usage_percent(150, 100), 100.0);
        assert_eq!(usage_percent(10, 0), 0.0);
        assert_eq!(usage_percent(1, 3), 33.33);
        assert_eq!(truncated_percent(33.339), 33.33);
        assert_eq!(percent_label(truncated_percent(33.339)), "33.33%");
    }

    #[test]
    fn usage_view_formats_used_total_and_detail() {
        let view = usage_view(512, 1024, "custom detail".to_owned());

        assert_eq!(view.percent, 50.0);
        assert_eq!(view.percent_label, "50.00%");
        assert_eq!(view.used_label, "512 B");
        assert_eq!(view.total_label, "1.00 KB");
        assert_eq!(view.detail, "custom detail");
    }

    #[test]
    fn rate_metric_formats_delta_and_updates_previous_sample() {
        let start = Instant::now();
        let mut old_busy = HashMap::new();
        old_busy.insert("sda".to_owned(), 100);
        let mut current_busy = HashMap::new();
        current_busy.insert("sda".to_owned(), 600);
        let mut previous = Some(CounterSample {
            read: 1_024,
            write: 2_048,
            busy_millis_by_device: old_busy,
            sampled_at: start,
        });

        let metric = rate_metric(
            &mut previous,
            Some(CounterSample {
                read: 3_072,
                write: 4_096,
                busy_millis_by_device: current_busy,
                sampled_at: start + std::time::Duration::from_secs(2),
            }),
            "read",
            "write",
            "unavailable",
        );

        assert_eq!(metric.read_label, "1.00 KB/s");
        assert_eq!(metric.write_label, "1.00 KB/s");
        assert_eq!(metric.utilization_percent, Some(25.0));
        assert_eq!(metric.utilization_label, "25.00%");
        assert_eq!(
            previous.as_ref().map(|sample| (sample.read, sample.write)),
            Some((3_072, 4_096))
        );
    }

    #[test]
    fn rate_metric_reports_unavailable_without_current_sample() {
        let mut previous = None;
        let metric = rate_metric(&mut previous, None, "read", "write", "unavailable");

        assert_eq!(metric.read_label, "--");
        assert_eq!(metric.write_label, "--");
        assert_eq!(metric.detail, "unavailable");
        assert_eq!(metric.utilization_percent, None);
        assert!(previous.is_none());
    }

    #[test]
    fn calculates_disk_busy_percent_without_rounding_up() {
        let mut old_busy = HashMap::new();
        old_busy.insert("sda".to_owned(), 1_000);
        let old = CounterSample {
            read: 0,
            write: 0,
            busy_millis_by_device: old_busy,
            sampled_at: Instant::now(),
        };

        let mut current_busy = HashMap::new();
        current_busy.insert("sda".to_owned(), 2_000);
        let current = CounterSample {
            read: 0,
            write: 0,
            busy_millis_by_device: current_busy,
            sampled_at: Instant::now(),
        };

        assert_eq!(disk_busy_percent(&old, &current, 3.0), Some(33.33));
    }

    #[test]
    fn disk_busy_percent_handles_missing_devices_and_zero_elapsed() {
        let mut old_busy = HashMap::new();
        old_busy.insert("sda".to_owned(), 1_000);
        let old = CounterSample {
            read: 0,
            write: 0,
            busy_millis_by_device: old_busy,
            sampled_at: Instant::now(),
        };

        let current = CounterSample {
            read: 0,
            write: 0,
            busy_millis_by_device: HashMap::from([("vdb".to_owned(), 2_000)]),
            sampled_at: Instant::now(),
        };

        assert_eq!(disk_busy_percent(&old, &current, 3.0), None);
        assert_eq!(disk_busy_percent(&old, &current, 0.0), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_df_line_with_mount_point() {
        let sample = parse_df_line("/ 107374182400 32212254720").expect("df line");
        assert_eq!(sample.total, 107_374_182_400);
        assert_eq!(sample.used, 32_212_254_720);
        assert_eq!(sample.mount_point, "/");
    }
}
