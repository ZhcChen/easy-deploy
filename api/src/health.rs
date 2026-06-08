use std::time::Duration;

use tokio::{net::TcpStream, time::timeout};
use url::Url;

use crate::deploy::{ComposeExecutor, DeployError, SystemdExecutor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthCheckKind {
    None,
    Http,
    Tcp,
    ComposeRunning,
    SystemdActive,
}

impl HealthCheckKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Http => "http",
            Self::Tcp => "tcp",
            Self::ComposeRunning => "compose_running",
            Self::SystemdActive => "systemd_active",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "不检查",
            Self::Http => "HTTP GET",
            Self::Tcp => "TCP 连接",
            Self::ComposeRunning => "容器运行状态",
            Self::SystemdActive => "systemd active",
        }
    }
}

impl TryFrom<&str> for HealthCheckKind {
    type Error = HealthError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "none" => Ok(Self::None),
            "http" => Ok(Self::Http),
            "tcp" => Ok(Self::Tcp),
            "compose_running" => Ok(Self::ComposeRunning),
            "systemd_active" => Ok(Self::SystemdActive),
            _ => Err(HealthError::InvalidInput("健康检查类型无效".to_owned())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HealthCheckConfig {
    pub kind: HealthCheckKind,
    pub endpoint: String,
    pub timeout_secs: u64,
    pub expected_status: u16,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            kind: HealthCheckKind::None,
            endpoint: String::new(),
            timeout_secs: 5,
            expected_status: 200,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HealthCheckOutcome {
    pub healthy: bool,
    pub message: String,
}

#[derive(Debug)]
pub enum HealthError {
    InvalidInput(String),
    CheckFailed(String),
}

impl HealthError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::CheckFailed(message) => message,
        }
    }
}

impl std::fmt::Display for HealthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HealthError {}

impl From<DeployError> for HealthError {
    fn from(value: DeployError) -> Self {
        Self::CheckFailed(value.message().to_owned())
    }
}

pub fn normalize_health_config(
    kind: &str,
    endpoint: &str,
    timeout_secs: i64,
    expected_status: i64,
) -> Result<HealthCheckConfig, HealthError> {
    let kind = HealthCheckKind::try_from(kind)?;
    let timeout_secs = timeout_secs.clamp(1, 60) as u64;
    let expected_status = expected_status.clamp(100, 599) as u16;
    let endpoint = endpoint.trim().to_owned();
    match kind {
        HealthCheckKind::None | HealthCheckKind::ComposeRunning => Ok(HealthCheckConfig {
            kind,
            endpoint: String::new(),
            timeout_secs,
            expected_status,
        }),
        HealthCheckKind::SystemdActive => {
            if endpoint.is_empty() {
                return Err(HealthError::InvalidInput(
                    "systemd active 检查需要填写 unit 名称".to_owned(),
                ));
            }
            Ok(HealthCheckConfig {
                kind,
                endpoint,
                timeout_secs,
                expected_status,
            })
        }
        HealthCheckKind::Http => {
            let url = Url::parse(&endpoint)
                .map_err(|_| HealthError::InvalidInput("HTTP 健康检查地址无效".to_owned()))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(HealthError::InvalidInput(
                    "HTTP 健康检查只支持 http 或 https".to_owned(),
                ));
            }
            Ok(HealthCheckConfig {
                kind,
                endpoint,
                timeout_secs,
                expected_status,
            })
        }
        HealthCheckKind::Tcp => {
            parse_tcp_endpoint(&endpoint)?;
            Ok(HealthCheckConfig {
                kind,
                endpoint,
                timeout_secs,
                expected_status,
            })
        }
    }
}

pub async fn run_health_check(
    config: &HealthCheckConfig,
    compose: &ComposeExecutor,
    systemd: &SystemdExecutor,
    work_dir: std::path::PathBuf,
) -> Result<HealthCheckOutcome, HealthError> {
    match config.kind {
        HealthCheckKind::None => Ok(HealthCheckOutcome {
            healthy: true,
            message: "未配置健康检查".to_owned(),
        }),
        HealthCheckKind::Http => run_http_check(config).await,
        HealthCheckKind::Tcp => run_tcp_check(config).await,
        HealthCheckKind::ComposeRunning => run_compose_running_check(compose, work_dir).await,
        HealthCheckKind::SystemdActive => run_systemd_active_check(config, systemd, work_dir).await,
    }
}

async fn run_http_check(config: &HealthCheckConfig) -> Result<HealthCheckOutcome, HealthError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|err| HealthError::CheckFailed(format!("创建 HTTP 客户端失败: {err}")))?;
    let response = client
        .get(&config.endpoint)
        .send()
        .await
        .map_err(|err| HealthError::CheckFailed(format!("HTTP 健康检查请求失败: {err}")))?;
    let status = response.status().as_u16();
    if status == config.expected_status {
        Ok(HealthCheckOutcome {
            healthy: true,
            message: format!("HTTP 健康检查通过: {status}"),
        })
    } else {
        Ok(HealthCheckOutcome {
            healthy: false,
            message: format!(
                "HTTP 健康检查失败: 返回 {status}，期望 {}",
                config.expected_status
            ),
        })
    }
}

async fn run_tcp_check(config: &HealthCheckConfig) -> Result<HealthCheckOutcome, HealthError> {
    let endpoint = parse_tcp_endpoint(&config.endpoint)?;
    let result = timeout(
        Duration::from_secs(config.timeout_secs),
        TcpStream::connect(endpoint.as_str()),
    )
    .await;
    match result {
        Ok(Ok(_stream)) => Ok(HealthCheckOutcome {
            healthy: true,
            message: format!("TCP 健康检查通过: {endpoint}"),
        }),
        Ok(Err(err)) => Ok(HealthCheckOutcome {
            healthy: false,
            message: format!("TCP 健康检查失败: {endpoint}: {err}"),
        }),
        Err(_) => Ok(HealthCheckOutcome {
            healthy: false,
            message: format!("TCP 健康检查超时: {endpoint}"),
        }),
    }
}

async fn run_compose_running_check(
    compose: &ComposeExecutor,
    work_dir: std::path::PathBuf,
) -> Result<HealthCheckOutcome, HealthError> {
    let output = compose.ps_running(work_dir).await?;
    if !output.success {
        return Ok(HealthCheckOutcome {
            healthy: false,
            message: output_summary(&output.output, "docker compose ps 返回失败"),
        });
    }
    let running_lines = output
        .output
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && !line.starts_with("NAME")
                && !line.starts_with("time=\"")
                && !line.starts_with("---")
        })
        .count();
    if running_lines > 0 {
        Ok(HealthCheckOutcome {
            healthy: true,
            message: format!("容器运行状态检查通过: {running_lines} 个运行中容器"),
        })
    } else {
        Ok(HealthCheckOutcome {
            healthy: false,
            message: "容器运行状态检查失败: 未发现运行中容器".to_owned(),
        })
    }
}

async fn run_systemd_active_check(
    config: &HealthCheckConfig,
    systemd: &SystemdExecutor,
    work_dir: std::path::PathBuf,
) -> Result<HealthCheckOutcome, HealthError> {
    let output = systemd.is_active(work_dir, &config.endpoint).await?;
    if output.success && output.output.trim() == "active" {
        Ok(HealthCheckOutcome {
            healthy: true,
            message: format!("systemd active 检查通过: {}", config.endpoint),
        })
    } else {
        Ok(HealthCheckOutcome {
            healthy: false,
            message: output_summary(
                &output.output,
                &format!("systemd active 检查失败: {}", config.endpoint),
            ),
        })
    }
}

fn parse_tcp_endpoint(value: &str) -> Result<String, HealthError> {
    let value = value.trim();
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(HealthError::InvalidInput(
            "TCP 健康检查地址格式应为 host:port".to_owned(),
        ));
    };
    if host.trim().is_empty() || port.parse::<u16>().is_err() {
        return Err(HealthError::InvalidInput(
            "TCP 健康检查地址格式应为 host:port".to_owned(),
        ));
    }
    Ok(format!("{}:{}", host.trim(), port.trim()))
}

fn output_summary(output: &str, fallback: &str) -> String {
    let summary = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("；");
    if summary.is_empty() {
        fallback.to_owned()
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tcp_config_accepts_host_port() {
        let config =
            normalize_health_config("tcp", "127.0.0.1:8080", 5, 200).expect("valid tcp config");

        assert_eq!(config.kind, HealthCheckKind::Tcp);
        assert_eq!(config.endpoint, "127.0.0.1:8080");
    }

    #[test]
    fn normalize_http_config_rejects_non_http_url() {
        let err = normalize_health_config("http", "file:///tmp/health", 5, 200)
            .expect_err("file URL should fail");

        assert_eq!(err.message(), "HTTP 健康检查只支持 http 或 https");
    }
}
