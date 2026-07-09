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
    previous_process_io: Option<ProcessIoSample>,
    previous_network: Option<CounterSample>,
}

impl HostMetricsCollector {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            previous_cpu: read_cpu_sample(),
            previous_disk: read_disk_io_sample(),
            previous_process_io: read_process_io_sample().map(|read| read.sample),
            previous_network: read_network_sample(),
        }
    }

    fn snapshot(&mut self) -> HostMetricsSnapshot {
        let cpu = cpu_metric(&mut self.previous_cpu);
        let memory = memory_metric();
        let disk = disk_usage_metric(&self.data_dir);
        let mut disk_rate = rate_metric(
            &mut self.previous_disk,
            read_disk_io_sample(),
            "读",
            "写",
            "磁盘速率暂不可用",
        );
        let (processes, process_detail) =
            process_rate_metric(&mut self.previous_process_io, read_process_io_sample());
        disk_rate.processes = processes;
        disk_rate.process_detail = process_detail;
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
    pub devices: Vec<RateDeviceMetric>,
    pub processes: Vec<RateProcessMetric>,
    pub process_detail: String,
}

#[derive(Debug, Serialize)]
pub struct RateDeviceMetric {
    pub name: String,
    pub read_label: String,
    pub write_label: String,
    pub total_label: String,
    pub utilization_percent: f64,
    pub utilization_label: String,
}

#[derive(Debug, Serialize)]
pub struct RateProcessMetric {
    pub pid: u32,
    pub name: String,
    pub command: String,
    pub container_id: Option<String>,
    pub read_label: String,
    pub write_label: String,
    pub total_label: String,
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
    devices: HashMap<String, DeviceCounterSample>,
    sampled_at: Instant,
}

#[derive(Clone, Copy)]
struct DeviceCounterSample {
    read: u64,
    write: u64,
    busy_millis: u64,
}

struct RawRateDeviceMetric {
    name: String,
    read_rate: f64,
    write_rate: f64,
    total_rate: f64,
    utilization_percent: f64,
}

#[derive(Clone)]
struct ProcessIoSample {
    processes: HashMap<u32, ProcessIoCounter>,
    sampled_at: Instant,
}

struct ProcessIoRead {
    sample: ProcessIoSample,
    permission_denied_count: usize,
}

#[derive(Clone)]
struct ProcessIoCounter {
    pid: u32,
    name: String,
    command: String,
    container_id: Option<String>,
    read_bytes: u64,
    write_bytes: u64,
}

struct RawRateProcessMetric {
    pid: u32,
    name: String,
    command: String,
    container_id: Option<String>,
    read_rate: f64,
    write_rate: f64,
    total_rate: f64,
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
            devices: Vec::new(),
            processes: Vec::new(),
            process_detail: String::new(),
        };
    };

    let (read_rate, write_rate, device_rates) = previous
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
                disk_rate_devices(old, &current, elapsed),
            )
        })
        .unwrap_or_else(|| (0.0, 0.0, Vec::new()));
    *previous = Some(current);

    let read_label = rate_label(read_rate);
    let write_label = rate_label(write_rate);
    let utilization_percent = device_rates
        .iter()
        .map(|device| device.utilization_percent)
        .reduce(f64::max);
    let utilization_label = utilization_percent
        .map(percent_label)
        .unwrap_or_else(|| "--".to_owned());
    let devices = device_rates
        .into_iter()
        .map(|device| RateDeviceMetric {
            name: device.name,
            read_label: rate_label(device.read_rate),
            write_label: rate_label(device.write_rate),
            total_label: rate_label(device.total_rate),
            utilization_percent: device.utilization_percent,
            utilization_label: percent_label(device.utilization_percent),
        })
        .collect();
    RateMetric {
        read_label: read_label.clone(),
        write_label: write_label.clone(),
        detail: format!("{read_label_prefix} {read_label} · {write_label_prefix} {write_label}"),
        utilization_percent,
        utilization_label,
        devices,
        processes: Vec::new(),
        process_detail: String::new(),
    }
}

fn process_rate_metric(
    previous: &mut Option<ProcessIoSample>,
    current: Option<ProcessIoRead>,
) -> (Vec<RateProcessMetric>, String) {
    let Some(current) = current else {
        *previous = None;
        return (Vec::new(), unsupported_detail());
    };

    let Some(old) = previous.as_ref() else {
        *previous = Some(current.sample);
        return (Vec::new(), "等待下一次进程 IO 采样".to_owned());
    };

    let elapsed = current
        .sample
        .sampled_at
        .duration_since(old.sampled_at)
        .as_secs_f64()
        .max(0.001);
    let mut process_rates = current
        .sample
        .processes
        .iter()
        .filter_map(|(pid, current_process)| {
            let old_process = old.processes.get(pid)?;
            let read_rate = current_process
                .read_bytes
                .saturating_sub(old_process.read_bytes) as f64
                / elapsed;
            let write_rate = current_process
                .write_bytes
                .saturating_sub(old_process.write_bytes) as f64
                / elapsed;
            let total_rate = read_rate + write_rate;
            if total_rate <= 0.0 {
                return None;
            }
            Some(RawRateProcessMetric {
                pid: current_process.pid,
                name: current_process.name.clone(),
                command: current_process.command.clone(),
                container_id: current_process.container_id.clone(),
                read_rate,
                write_rate,
                total_rate,
            })
        })
        .collect::<Vec<_>>();

    process_rates.sort_by(|left, right| {
        right
            .total_rate
            .total_cmp(&left.total_rate)
            .then_with(|| right.write_rate.total_cmp(&left.write_rate))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.pid.cmp(&right.pid))
    });
    process_rates.truncate(20);
    *previous = Some(current.sample);

    let detail = if process_rates.is_empty() && current.permission_denied_count > 0 {
        format!(
            "未采集到可读进程 IO，已跳过 {} 个无权限进程",
            current.permission_denied_count
        )
    } else if process_rates.is_empty() {
        "暂无进程读写".to_owned()
    } else if current.permission_denied_count > 0 {
        format!(
            "按总读写速率排序，已跳过 {} 个无权限进程",
            current.permission_denied_count
        )
    } else {
        "按总读写速率排序".to_owned()
    };

    (
        process_rates
            .into_iter()
            .map(|process| RateProcessMetric {
                pid: process.pid,
                name: process.name,
                command: process.command,
                container_id: process.container_id,
                read_label: rate_label(process.read_rate),
                write_label: rate_label(process.write_rate),
                total_label: rate_label(process.total_rate),
            })
            .collect(),
        detail,
    )
}

fn disk_rate_devices(
    old: &CounterSample,
    current: &CounterSample,
    elapsed_secs: f64,
) -> Vec<RawRateDeviceMetric> {
    if current.devices.is_empty() || elapsed_secs <= 0.0 {
        return Vec::new();
    }

    let elapsed_millis = elapsed_secs * 1000.0;
    let mut devices = current
        .devices
        .iter()
        .filter_map(|(name, current_device)| {
            let old_device = old.devices.get(name)?;
            let read_rate =
                current_device.read.saturating_sub(old_device.read) as f64 / elapsed_secs;
            let write_rate =
                current_device.write.saturating_sub(old_device.write) as f64 / elapsed_secs;
            let utilization_percent = truncated_percent(
                current_device
                    .busy_millis
                    .saturating_sub(old_device.busy_millis) as f64
                    / elapsed_millis
                    * 100.0,
            );
            Some(RawRateDeviceMetric {
                name: name.to_owned(),
                read_rate,
                write_rate,
                total_rate: read_rate + write_rate,
                utilization_percent,
            })
        })
        .collect::<Vec<_>>();

    devices.sort_by(|left, right| {
        right
            .utilization_percent
            .total_cmp(&left.utilization_percent)
            .then_with(|| right.total_rate.total_cmp(&left.total_rate))
            .then_with(|| left.name.cmp(&right.name))
    });
    devices.truncate(20);
    devices
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
    let mut devices = HashMap::new();

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
        let device_read = sectors_read.saturating_mul(512);
        let device_write = sectors_written.saturating_mul(512);
        read = read.saturating_add(device_read);
        write = write.saturating_add(device_write);
        devices.insert(
            name.to_owned(),
            DeviceCounterSample {
                read: device_read,
                write: device_write,
                busy_millis,
            },
        );
    }

    Some(CounterSample {
        read,
        write,
        devices,
        sampled_at: Instant::now(),
    })
}

#[cfg(not(target_os = "linux"))]
fn read_disk_io_sample() -> Option<CounterSample> {
    None
}

#[cfg(target_os = "linux")]
fn read_process_io_sample() -> Option<ProcessIoRead> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut processes = HashMap::new();
    let mut permission_denied_count = 0_usize;

    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process_dir = entry.path();
        let io_path = process_dir.join("io");
        let io_content = match std::fs::read_to_string(&io_path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                permission_denied_count += 1;
                continue;
            }
            Err(_) => continue,
        };
        let Some((read_bytes, write_bytes)) = parse_process_io_bytes(&io_content) else {
            continue;
        };
        let name = read_process_name(&process_dir).unwrap_or_else(|| pid.to_string());
        let command = read_process_command(&process_dir).unwrap_or_else(|| name.clone());
        let container_id = read_process_container_id(&process_dir);
        processes.insert(
            pid,
            ProcessIoCounter {
                pid,
                name,
                command,
                container_id,
                read_bytes,
                write_bytes,
            },
        );
    }

    Some(ProcessIoRead {
        sample: ProcessIoSample {
            processes,
            sampled_at: Instant::now(),
        },
        permission_denied_count,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_process_io_sample() -> Option<ProcessIoRead> {
    None
}

#[cfg(target_os = "linux")]
fn parse_process_io_bytes(content: &str) -> Option<(u64, u64)> {
    let mut read_bytes = None;
    let mut write_bytes = None;
    for line in content.lines() {
        if let Some(value) = parse_proc_io_value(line, "read_bytes:") {
            read_bytes = Some(value);
        } else if let Some(value) = parse_proc_io_value(line, "write_bytes:") {
            write_bytes = Some(value);
        }
    }
    Some((read_bytes?, write_bytes?))
}

#[cfg(target_os = "linux")]
fn parse_proc_io_value(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?.trim().parse::<u64>().ok()
}

#[cfg(target_os = "linux")]
fn read_process_name(process_dir: &Path) -> Option<String> {
    let value = std::fs::read_to_string(process_dir.join("comm")).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| truncate_chars(value, 80))
}

#[cfg(target_os = "linux")]
fn read_process_command(process_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(process_dir.join("cmdline")).ok()?;
    let parts = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    Some(truncate_chars(&parts.join(" "), 220))
}

#[cfg(target_os = "linux")]
fn read_process_container_id(process_dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(process_dir.join("cgroup")).ok()?;
    content.lines().find_map(short_container_id)
}

#[cfg(target_os = "linux")]
fn short_container_id(line: &str) -> Option<String> {
    line.split(|ch: char| !ch.is_ascii_hexdigit())
        .find(|token| token.len() >= 64 && token.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|token| token[..12].to_owned())
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
        devices: HashMap::new(),
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

#[cfg(any(target_os = "linux", test))]
fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
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

    type ProcessIoFixture<'a> = (u32, &'a str, &'a str, Option<&'a str>, u64, u64);

    fn counter_sample(
        read: u64,
        write: u64,
        devices: Vec<(&str, u64, u64, u64)>,
        sampled_at: Instant,
    ) -> CounterSample {
        CounterSample {
            read,
            write,
            devices: devices
                .into_iter()
                .map(|(name, read, write, busy_millis)| {
                    (
                        name.to_owned(),
                        DeviceCounterSample {
                            read,
                            write,
                            busy_millis,
                        },
                    )
                })
                .collect(),
            sampled_at,
        }
    }

    fn process_io_read(
        processes: Vec<ProcessIoFixture<'_>>,
        permission_denied_count: usize,
        sampled_at: Instant,
    ) -> ProcessIoRead {
        ProcessIoRead {
            sample: ProcessIoSample {
                processes: processes
                    .into_iter()
                    .map(
                        |(pid, name, command, container_id, read_bytes, write_bytes)| {
                            (
                                pid,
                                ProcessIoCounter {
                                    pid,
                                    name: name.to_owned(),
                                    command: command.to_owned(),
                                    container_id: container_id.map(ToOwned::to_owned),
                                    read_bytes,
                                    write_bytes,
                                },
                            )
                        },
                    )
                    .collect(),
                sampled_at,
            },
            permission_denied_count,
        }
    }

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
        let mut previous = Some(counter_sample(
            1_024,
            2_048,
            vec![("sda", 1_024, 2_048, 100)],
            start,
        ));

        let metric = rate_metric(
            &mut previous,
            Some(counter_sample(
                3_072,
                4_096,
                vec![("sda", 3_072, 4_096, 600)],
                start + std::time::Duration::from_secs(2),
            )),
            "read",
            "write",
            "unavailable",
        );

        assert_eq!(metric.read_label, "1.00 KB/s");
        assert_eq!(metric.write_label, "1.00 KB/s");
        assert_eq!(metric.utilization_percent, Some(25.0));
        assert_eq!(metric.utilization_label, "25.00%");
        assert_eq!(metric.devices.len(), 1);
        assert_eq!(metric.devices[0].name, "sda");
        assert_eq!(metric.devices[0].total_label, "2.00 KB/s");
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
        assert!(metric.devices.is_empty());
        assert!(metric.processes.is_empty());
        assert!(previous.is_none());
    }

    #[test]
    fn disk_rate_devices_calculates_busy_percent_without_rounding_up() {
        let old = counter_sample(0, 0, vec![("sda", 0, 0, 1_000)], Instant::now());
        let current = counter_sample(0, 0, vec![("sda", 0, 0, 2_000)], Instant::now());
        let devices = disk_rate_devices(&old, &current, 3.0);

        assert_eq!(devices[0].utilization_percent, 33.33);
    }

    #[test]
    fn disk_rate_devices_handle_missing_devices_and_zero_elapsed() {
        let old = counter_sample(0, 0, vec![("sda", 0, 0, 1_000)], Instant::now());
        let current = counter_sample(0, 0, vec![("vdb", 0, 0, 2_000)], Instant::now());

        assert!(disk_rate_devices(&old, &current, 3.0).is_empty());
        assert!(disk_rate_devices(&old, &current, 0.0).is_empty());
    }

    #[test]
    fn disk_rate_devices_returns_top_20_by_busy_then_total_rate() {
        let mut old_devices = HashMap::new();
        let mut current_devices = HashMap::new();
        for index in 0..25 {
            let name = format!("sd{index:02}");
            old_devices.insert(
                name.clone(),
                DeviceCounterSample {
                    read: 0,
                    write: 0,
                    busy_millis: 0,
                },
            );
            current_devices.insert(
                name,
                DeviceCounterSample {
                    read: (index + 1) * 512,
                    write: 0,
                    busy_millis: (index + 1) * 10,
                },
            );
        }
        old_devices.insert(
            "fast-tie".to_owned(),
            DeviceCounterSample {
                read: 0,
                write: 0,
                busy_millis: 0,
            },
        );
        current_devices.insert(
            "fast-tie".to_owned(),
            DeviceCounterSample {
                read: 16_384,
                write: 16_384,
                busy_millis: 250,
            },
        );

        let old = CounterSample {
            read: 0,
            write: 0,
            devices: old_devices,
            sampled_at: Instant::now(),
        };
        let current = CounterSample {
            read: 0,
            write: 0,
            devices: current_devices,
            sampled_at: Instant::now(),
        };

        let devices = disk_rate_devices(&old, &current, 1.0);

        assert_eq!(devices.len(), 20);
        assert_eq!(devices[0].name, "fast-tie");
        assert_eq!(devices[0].utilization_percent, 25.0);
        assert_eq!(devices[1].name, "sd24");
        assert_eq!(devices[19].name, "sd06");
    }

    #[test]
    fn process_rate_metric_waits_for_second_sample() {
        let start = Instant::now();
        let mut previous = None;
        let (processes, detail) = process_rate_metric(
            &mut previous,
            Some(process_io_read(
                vec![(42, "api", "easy-deploy-api", None, 1_024, 2_048)],
                0,
                start,
            )),
        );

        assert!(processes.is_empty());
        assert_eq!(detail, "等待下一次进程 IO 采样");
        assert!(previous.is_some());
    }

    #[test]
    fn process_rate_metric_sorts_by_total_disk_rate() {
        let start = Instant::now();
        let mut previous = Some(
            process_io_read(
                vec![
                    (42, "api", "easy-deploy-api", None, 1_024, 2_048),
                    (
                        77,
                        "worker",
                        "worker --job",
                        Some("abcdef123456"),
                        4_096,
                        4_096,
                    ),
                    (88, "idle", "idle", None, 0, 0),
                ],
                0,
                start,
            )
            .sample,
        );

        let (processes, detail) = process_rate_metric(
            &mut previous,
            Some(process_io_read(
                vec![
                    (42, "api", "easy-deploy-api", None, 3_072, 4_096),
                    (
                        77,
                        "worker",
                        "worker --job",
                        Some("abcdef123456"),
                        4_096,
                        12_288,
                    ),
                    (88, "idle", "idle", None, 0, 0),
                ],
                2,
                start + std::time::Duration::from_secs(2),
            )),
        );

        assert_eq!(detail, "按总读写速率排序，已跳过 2 个无权限进程");
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 77);
        assert_eq!(processes[0].name, "worker");
        assert_eq!(processes[0].container_id.as_deref(), Some("abcdef123456"));
        assert_eq!(processes[0].read_label, "0 B/s");
        assert_eq!(processes[0].write_label, "4.00 KB/s");
        assert_eq!(processes[0].total_label, "4.00 KB/s");
        assert_eq!(processes[1].pid, 42);
        assert_eq!(processes[1].total_label, "2.00 KB/s");
    }

    #[test]
    fn process_rate_metric_reports_permission_only_result() {
        let start = Instant::now();
        let mut previous = Some(
            process_io_read(
                vec![(42, "api", "easy-deploy-api", None, 1_024, 2_048)],
                0,
                start,
            )
            .sample,
        );

        let (processes, detail) = process_rate_metric(
            &mut previous,
            Some(process_io_read(
                vec![(42, "api", "easy-deploy-api", None, 1_024, 2_048)],
                12,
                start + std::time::Duration::from_secs(1),
            )),
        );

        assert!(processes.is_empty());
        assert_eq!(detail, "未采集到可读进程 IO，已跳过 12 个无权限进程");
    }

    #[test]
    fn truncates_long_text_by_characters() {
        assert_eq!(truncate_chars("abcdef", 3), "abc...");
        assert_eq!(truncate_chars("中文命令", 2), "中文...");
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_df_line_with_mount_point() {
        let sample = parse_df_line("/ 107374182400 32212254720").expect("df line");
        assert_eq!(sample.total, 107_374_182_400);
        assert_eq!(sample.used, 32_212_254_720);
        assert_eq!(sample.mount_point, "/");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_process_io_and_container_id() {
        let io = "rchar: 9\nwchar: 8\nread_bytes: 1024\nwrite_bytes: 2048\n";
        assert_eq!(parse_process_io_bytes(io), Some((1_024, 2_048)));
        assert_eq!(
            short_container_id(
                "0::/system.slice/docker-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.scope"
            )
            .as_deref(),
            Some("0123456789ab")
        );
    }
}
