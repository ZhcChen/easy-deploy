use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{fs, io::AsyncWriteExt, process::Command, time::timeout};

use crate::settings::DEFAULT_COMMAND_TIMEOUT_SECS;

pub type DynCommandRunner = Arc<dyn CommandRunner>;

#[derive(Debug)]
pub enum DeployError {
    InvalidInput(String),
    Command(String),
}

impl DeployError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Command(message) => message,
        }
    }
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DeployError {}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CommandResult {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn success(&self) -> bool {
        self.status_code == Some(0)
    }

    pub fn combined_output(&self) -> String {
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout.to_owned(),
            (true, false) => stderr.to_owned(),
            (false, false) => format!("{stdout}\n{stderr}"),
        }
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError>;
}

pub struct TokioCommandRunner {
    timeout: Duration,
}

impl TokioCommandRunner {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }
}

impl Default for TokioCommandRunner {
    fn default() -> Self {
        Self::new(DEFAULT_COMMAND_TIMEOUT_SECS)
    }
}

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
        let command = render_command(&spec.program, &spec.args);
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| DeployError::Command(format!("执行命令 {command} 失败: {err}")))?;
        let output = match timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => output
                .map_err(|err| DeployError::Command(format!("执行命令 {command} 失败: {err}")))?,
            Err(_) => {
                return Err(DeployError::Command(format!(
                    "执行命令 {command} 超时（{} 秒）",
                    self.timeout.as_secs()
                )));
            }
        };
        Ok(CommandResult {
            status_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

#[derive(Clone)]
pub struct ComposeExecutor {
    runner: DynCommandRunner,
}

#[derive(Clone)]
pub struct SystemdExecutor {
    runner: DynCommandRunner,
    ssh_known_hosts_file: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SshExecutor {
    runner: DynCommandRunner,
    known_hosts_file: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SshTarget {
    user: String,
    address: String,
    port: u16,
    identity_file: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ComposeCommandOutput {
    pub command: String,
    pub success: bool,
    pub status_code: Option<i32>,
    pub output: String,
}

pub type CommandOutput = ComposeCommandOutput;

#[derive(Clone, Debug)]
pub struct SshKnownHostResult {
    pub lookup_key: String,
    pub known_hosts_file: PathBuf,
    pub added: bool,
}

pub fn ssh_known_hosts_file(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join("ssh").join("known_hosts")
}

impl ComposeExecutor {
    pub fn new(runner: DynCommandRunner) -> Self {
        Self { runner }
    }

    pub async fn docker_info(
        &self,
        work_dir: PathBuf,
    ) -> Result<ComposeCommandOutput, DeployError> {
        self.run_docker(work_dir, vec!["info".to_owned()]).await
    }

    pub async fn config(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.run_compose(work_dir, &["config"]).await
    }

    pub async fn up(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.run_compose(work_dir, &["up", "-d", "--remove-orphans"])
            .await
    }

    pub async fn down(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.run_compose(work_dir, &["down"]).await
    }

    pub async fn restart(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.run_compose(work_dir, &["restart"]).await
    }

    pub async fn logs(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.logs_with_tail(work_dir, 200).await
    }

    pub async fn logs_with_tail(
        &self,
        work_dir: PathBuf,
        tail_lines: u16,
    ) -> Result<ComposeCommandOutput, DeployError> {
        let tail_lines = normalize_log_tail_lines(tail_lines);
        self.run_compose_owned(
            work_dir,
            vec![
                "logs".to_owned(),
                "--tail".to_owned(),
                tail_lines.to_string(),
                "--no-color".to_owned(),
            ],
        )
        .await
    }

    pub async fn service_logs(
        &self,
        work_dir: PathBuf,
        service_name: &str,
    ) -> Result<ComposeCommandOutput, DeployError> {
        self.service_logs_with_tail(work_dir, service_name, 200)
            .await
    }

    pub async fn service_logs_with_tail(
        &self,
        work_dir: PathBuf,
        service_name: &str,
        tail_lines: u16,
    ) -> Result<ComposeCommandOutput, DeployError> {
        let service_name = normalize_compose_service_name(service_name)?;
        let tail_lines = normalize_log_tail_lines(tail_lines);
        self.run_compose_owned(
            work_dir,
            vec![
                "logs".to_owned(),
                "--tail".to_owned(),
                tail_lines.to_string(),
                "--no-color".to_owned(),
                service_name,
            ],
        )
        .await
    }

    pub async fn ps_running(&self, work_dir: PathBuf) -> Result<ComposeCommandOutput, DeployError> {
        self.run_compose(work_dir, &["ps", "--status", "running"])
            .await
    }

    async fn run_compose(
        &self,
        work_dir: PathBuf,
        args: &[&str],
    ) -> Result<ComposeCommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "Compose 工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let mut docker_args = vec!["compose".to_owned()];
        docker_args.extend(args);
        self.run_docker(work_dir, docker_args).await
    }

    async fn run_compose_owned(
        &self,
        work_dir: PathBuf,
        args: Vec<String>,
    ) -> Result<ComposeCommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "Compose 工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let mut docker_args = vec!["compose".to_owned()];
        docker_args.extend(args);
        self.run_docker(work_dir, docker_args).await
    }

    async fn run_docker(
        &self,
        work_dir: PathBuf,
        docker_args: Vec<String>,
    ) -> Result<ComposeCommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "Compose 工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let command = render_command("docker", &docker_args);
        let result = self
            .runner
            .run(CommandSpec {
                program: "docker".to_owned(),
                args: docker_args,
                current_dir: work_dir,
            })
            .await?;
        Ok(ComposeCommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }
}

impl SystemdExecutor {
    pub fn new(runner: DynCommandRunner) -> Self {
        Self {
            runner,
            ssh_known_hosts_file: None,
        }
    }

    pub fn with_ssh_known_hosts_file(mut self, known_hosts_file: impl Into<PathBuf>) -> Self {
        self.ssh_known_hosts_file = Some(known_hosts_file.into());
        self
    }

    pub fn ssh_executor(&self) -> SshExecutor {
        let mut executor = SshExecutor::new(self.runner.clone());
        if let Some(known_hosts_file) = &self.ssh_known_hosts_file {
            executor = executor.with_known_hosts_file(known_hosts_file.clone());
        }
        executor
    }

    pub async fn daemon_reload(&self, work_dir: PathBuf) -> Result<CommandOutput, DeployError> {
        self.run_systemctl(work_dir, vec!["daemon-reload".to_owned()])
            .await
    }

    pub async fn link_unit(
        &self,
        work_dir: PathBuf,
        unit_path: PathBuf,
    ) -> Result<CommandOutput, DeployError> {
        if !unit_path.is_file() {
            return Err(DeployError::InvalidInput(format!(
                "systemd unit 文件不存在: {}",
                unit_path.to_string_lossy()
            )));
        }
        self.run_systemctl(
            work_dir,
            vec!["link".to_owned(), unit_path.to_string_lossy().to_string()],
        )
        .await
    }

    pub async fn make_executable(
        &self,
        work_dir: PathBuf,
        artifact_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let artifact_path = normalize_local_absolute_path(artifact_path)?;
        self.run_command("chmod", work_dir, vec!["+x".to_owned(), artifact_path])
            .await
    }

    pub async fn restart(
        &self,
        work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(work_dir, vec!["restart".to_owned(), unit_name])
            .await
    }

    pub async fn stop(
        &self,
        work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(work_dir, vec!["stop".to_owned(), unit_name])
            .await
    }

    pub async fn is_active(
        &self,
        work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(work_dir, vec!["is-active".to_owned(), unit_name])
            .await
    }

    pub async fn logs(
        &self,
        work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.logs_with_tail(work_dir, unit_name, 200).await
    }

    pub async fn logs_with_tail(
        &self,
        work_dir: PathBuf,
        unit_name: &str,
        tail_lines: u16,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        let tail_lines = normalize_log_tail_lines(tail_lines);
        self.run_journalctl(
            work_dir,
            vec![
                "-u".to_owned(),
                unit_name,
                "-n".to_owned(),
                tail_lines.to_string(),
                "--no-pager".to_owned(),
            ],
        )
        .await
    }

    pub async fn caddy_validate(
        &self,
        work_dir: PathBuf,
        config_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let config_path = normalize_local_absolute_path(config_path)?;
        self.run_command(
            "caddy",
            work_dir,
            vec![
                "validate".to_owned(),
                "--adapter".to_owned(),
                "caddyfile".to_owned(),
                "--config".to_owned(),
                config_path,
            ],
        )
        .await
    }

    pub async fn nginx_validate(
        &self,
        work_dir: PathBuf,
        config_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let config_path = normalize_local_absolute_path(config_path)?;
        self.run_command(
            "nginx",
            work_dir,
            vec!["-t".to_owned(), "-c".to_owned(), config_path],
        )
        .await
    }

    pub async fn reload_service(
        &self,
        work_dir: PathBuf,
        service_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let service_name = normalize_systemd_unit_name(service_name)?;
        self.run_systemctl(work_dir, vec!["reload".to_owned(), service_name])
            .await
    }

    async fn run_systemctl(
        &self,
        work_dir: PathBuf,
        args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "二进制工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let command = render_command("systemctl", &args);
        let result = self
            .runner
            .run(CommandSpec {
                program: "systemctl".to_owned(),
                args,
                current_dir: work_dir,
            })
            .await?;
        Ok(CommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }

    async fn run_journalctl(
        &self,
        work_dir: PathBuf,
        args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "二进制工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let command = render_command("journalctl", &args);
        let result = self
            .runner
            .run(CommandSpec {
                program: "journalctl".to_owned(),
                args,
                current_dir: work_dir,
            })
            .await?;
        Ok(CommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }

    async fn run_command(
        &self,
        program: &str,
        work_dir: PathBuf,
        args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "二进制工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let command = render_command(program, &args);
        let result = self
            .runner
            .run(CommandSpec {
                program: program.to_owned(),
                args,
                current_dir: work_dir,
            })
            .await?;
        Ok(CommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }
}

impl SshExecutor {
    pub fn new(runner: DynCommandRunner) -> Self {
        Self {
            runner,
            known_hosts_file: None,
        }
    }

    pub fn with_known_hosts_file(mut self, known_hosts_file: impl Into<PathBuf>) -> Self {
        self.known_hosts_file = Some(known_hosts_file.into());
        self
    }

    pub async fn mkdir_all(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let remote_path = normalize_remote_absolute_path(remote_path)?;
        self.run_ssh(
            target,
            local_work_dir,
            vec!["mkdir".to_owned(), "-p".to_owned(), remote_path],
        )
        .await
    }

    pub async fn copy_file(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        local_path: PathBuf,
        remote_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        if !local_path.is_file() {
            return Err(DeployError::InvalidInput(format!(
                "本地待同步文件不存在: {}",
                local_path.to_string_lossy()
            )));
        }
        self.ensure_target_known_host(target).await?;
        let remote_path = normalize_remote_absolute_path(remote_path)?;
        let mut args = vec!["-P".to_owned(), target.port.to_string()];
        append_ssh_known_hosts_args(&mut args, self.known_hosts_file.as_deref());
        if let Some(identity_file) = target.identity_file_arg() {
            args.push("-i".to_owned());
            args.push(identity_file);
            args.push("-o".to_owned());
            args.push("IdentitiesOnly=yes".to_owned());
        }
        args.push(local_path.to_string_lossy().to_string());
        args.push(format!("{}:{remote_path}", target.destination()));
        self.run_command("scp", local_work_dir, args).await
    }

    pub async fn compose_config(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec!["config".to_owned()],
        )
        .await
    }

    pub async fn compose_up(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec![
                "up".to_owned(),
                "-d".to_owned(),
                "--remove-orphans".to_owned(),
            ],
        )
        .await
    }

    pub async fn compose_down(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec!["down".to_owned()],
        )
        .await
    }

    pub async fn compose_restart(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec!["restart".to_owned()],
        )
        .await
    }

    pub async fn compose_ps_running(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec!["ps".to_owned(), "--status".to_owned(), "running".to_owned()],
        )
        .await
    }

    pub async fn compose_service_logs(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
        service_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.compose_service_logs_with_tail(
            target,
            local_work_dir,
            remote_work_dir,
            service_name,
            200,
        )
        .await
    }

    pub async fn compose_service_logs_with_tail(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
        service_name: &str,
        tail_lines: u16,
    ) -> Result<CommandOutput, DeployError> {
        let service_name = normalize_compose_service_name(service_name)?;
        let tail_lines = normalize_log_tail_lines(tail_lines);
        self.run_remote_compose(
            target,
            local_work_dir,
            remote_work_dir,
            vec![
                "logs".to_owned(),
                "--tail".to_owned(),
                tail_lines.to_string(),
                "--no-color".to_owned(),
                service_name,
            ],
        )
        .await
    }

    pub async fn http_health_check(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        endpoint: &str,
        timeout_secs: u64,
    ) -> Result<CommandOutput, DeployError> {
        let endpoint = normalize_remote_health_endpoint(endpoint, "HTTP")?;
        self.run_ssh(
            target,
            local_work_dir,
            vec![
                "curl".to_owned(),
                "-sS".to_owned(),
                "-L".to_owned(),
                "-o".to_owned(),
                "/dev/null".to_owned(),
                "-w".to_owned(),
                "%{http_code}".to_owned(),
                "--max-time".to_owned(),
                timeout_secs.clamp(1, 60).to_string(),
                "--connect-timeout".to_owned(),
                timeout_secs.clamp(1, 60).to_string(),
                endpoint,
            ],
        )
        .await
    }

    pub async fn tcp_health_check(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        endpoint: &str,
        timeout_secs: u64,
    ) -> Result<CommandOutput, DeployError> {
        let (host, port) = normalize_remote_tcp_endpoint(endpoint)?;
        self.run_ssh(
            target,
            local_work_dir,
            vec![
                "nc".to_owned(),
                "-z".to_owned(),
                "-w".to_owned(),
                timeout_secs.clamp(1, 60).to_string(),
                host,
                port,
            ],
        )
        .await
    }

    pub async fn daemon_reload(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
    ) -> Result<CommandOutput, DeployError> {
        self.run_systemctl(target, local_work_dir, vec!["daemon-reload".to_owned()])
            .await
    }

    pub async fn link_unit(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_unit_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let remote_unit_path = normalize_remote_absolute_path(remote_unit_path)?;
        self.run_systemctl(
            target,
            local_work_dir,
            vec!["link".to_owned(), remote_unit_path],
        )
        .await
    }

    pub async fn make_executable(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_artifact_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let remote_artifact_path = normalize_remote_absolute_path(remote_artifact_path)?;
        self.run_ssh(
            target,
            local_work_dir,
            vec!["chmod".to_owned(), "+x".to_owned(), remote_artifact_path],
        )
        .await
    }

    pub async fn restart(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(
            target,
            local_work_dir,
            vec!["restart".to_owned(), unit_name],
        )
        .await
    }

    pub async fn stop(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(target, local_work_dir, vec!["stop".to_owned(), unit_name])
            .await
    }

    pub async fn is_active(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        self.run_systemctl(
            target,
            local_work_dir,
            vec!["is-active".to_owned(), unit_name],
        )
        .await
    }

    pub async fn logs(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        unit_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        self.logs_with_tail(target, local_work_dir, unit_name, 200)
            .await
    }

    pub async fn logs_with_tail(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        unit_name: &str,
        tail_lines: u16,
    ) -> Result<CommandOutput, DeployError> {
        let unit_name = normalize_systemd_unit_name(unit_name)?;
        let tail_lines = normalize_log_tail_lines(tail_lines);
        self.run_journalctl(
            target,
            local_work_dir,
            vec![
                "-u".to_owned(),
                unit_name,
                "-n".to_owned(),
                tail_lines.to_string(),
                "--no-pager".to_owned(),
            ],
        )
        .await
    }

    pub async fn caddy_validate(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        config_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let config_path = normalize_remote_absolute_path(config_path)?;
        self.run_ssh(
            target,
            local_work_dir,
            vec![
                "caddy".to_owned(),
                "validate".to_owned(),
                "--adapter".to_owned(),
                "caddyfile".to_owned(),
                "--config".to_owned(),
                config_path,
            ],
        )
        .await
    }

    pub async fn nginx_validate(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        config_path: &str,
    ) -> Result<CommandOutput, DeployError> {
        let config_path = normalize_remote_absolute_path(config_path)?;
        self.run_ssh(
            target,
            local_work_dir,
            vec![
                "nginx".to_owned(),
                "-t".to_owned(),
                "-c".to_owned(),
                config_path,
            ],
        )
        .await
    }

    pub async fn reload_service(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        service_name: &str,
    ) -> Result<CommandOutput, DeployError> {
        let service_name = normalize_systemd_unit_name(service_name)?;
        self.run_systemctl(
            target,
            local_work_dir,
            vec!["reload".to_owned(), service_name],
        )
        .await
    }

    async fn run_systemctl(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        systemctl_args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        let mut args = vec!["systemctl".to_owned()];
        args.extend(systemctl_args);
        self.run_ssh(target, local_work_dir, args).await
    }

    async fn run_journalctl(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        journalctl_args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        let mut args = vec!["journalctl".to_owned()];
        args.extend(journalctl_args);
        self.run_ssh(target, local_work_dir, args).await
    }

    async fn run_remote_compose(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
        compose_args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        let remote_work_dir = normalize_remote_absolute_path(remote_work_dir)?;
        let mut args = vec![
            "cd".to_owned(),
            remote_work_dir,
            "&&".to_owned(),
            "docker".to_owned(),
            "compose".to_owned(),
        ];
        args.extend(compose_args);
        self.run_ssh(target, local_work_dir, args).await
    }

    async fn run_ssh(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        self.ensure_target_known_host(target).await?;
        let mut args = vec!["-p".to_owned(), target.port.to_string()];
        append_ssh_known_hosts_args(&mut args, self.known_hosts_file.as_deref());
        if let Some(identity_file) = target.identity_file_arg() {
            args.push("-i".to_owned());
            args.push(identity_file);
            args.push("-o".to_owned());
            args.push("IdentitiesOnly=yes".to_owned());
        }
        args.push(target.destination());
        args.extend(remote_args);
        self.run_command("ssh", local_work_dir, args).await
    }

    async fn ensure_target_known_host(&self, target: &SshTarget) -> Result<(), DeployError> {
        if let Some(known_hosts_file) = &self.known_hosts_file {
            ensure_ssh_known_host(
                &self.runner,
                known_hosts_file,
                target.address(),
                i64::from(target.port()),
            )
            .await?;
        }
        Ok(())
    }

    async fn run_command(
        &self,
        program: &str,
        local_work_dir: PathBuf,
        args: Vec<String>,
    ) -> Result<CommandOutput, DeployError> {
        if !local_work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "本地工作目录不存在: {}",
                local_work_dir.to_string_lossy()
            )));
        }
        let command = render_command(program, &args);
        let result = self
            .runner
            .run(CommandSpec {
                program: program.to_owned(),
                args,
                current_dir: local_work_dir,
            })
            .await?;
        Ok(CommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }
}

impl SshTarget {
    pub fn new(user: &str, address: &str, port: i64) -> Result<Self, DeployError> {
        let user = normalize_ssh_user(user)?;
        let address = normalize_ssh_address(address)?;
        let port = if (1..=65535).contains(&port) {
            port as u16
        } else {
            return Err(DeployError::InvalidInput(
                "SSH 端口需要在 1 到 65535 之间".to_owned(),
            ));
        };
        Ok(Self {
            user,
            address,
            port,
            identity_file: None,
        })
    }

    pub fn with_identity_file(mut self, identity_file: Option<PathBuf>) -> Self {
        self.identity_file = identity_file.filter(|path| !path.as_os_str().is_empty());
        self
    }

    pub fn identity_file(&self) -> Option<&PathBuf> {
        self.identity_file.as_ref()
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn destination(&self) -> String {
        format!("{}@{}", self.user, self.address)
    }

    fn identity_file_arg(&self) -> Option<String> {
        self.identity_file
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
    }
}

pub async fn ensure_ssh_known_host(
    runner: &DynCommandRunner,
    known_hosts_file: &Path,
    address: &str,
    port: i64,
) -> Result<SshKnownHostResult, DeployError> {
    let address = normalize_ssh_address(address)?;
    let port = if (1..=65535).contains(&port) {
        port as u16
    } else {
        return Err(DeployError::InvalidInput(
            "SSH 端口需要在 1 到 65535 之间".to_owned(),
        ));
    };
    let lookup_key = ssh_known_host_lookup_key(&address, port);
    ensure_known_hosts_parent(known_hosts_file).await?;
    ensure_known_hosts_file(known_hosts_file).await?;
    if known_host_entry_exists(runner, known_hosts_file, &lookup_key).await? {
        return Ok(SshKnownHostResult {
            lookup_key,
            known_hosts_file: known_hosts_file.to_path_buf(),
            added: false,
        });
    }
    append_known_host_entry(runner, known_hosts_file, &address, port).await?;
    if !known_host_entry_exists(runner, known_hosts_file, &lookup_key).await? {
        return Err(DeployError::Command(format!(
            "已采集 SSH 主机指纹，但未能写入 known_hosts: {lookup_key}"
        )));
    }
    Ok(SshKnownHostResult {
        lookup_key,
        known_hosts_file: known_hosts_file.to_path_buf(),
        added: true,
    })
}

pub fn ssh_known_host_lookup_key(address: &str, port: u16) -> String {
    if port == 22 {
        address.to_owned()
    } else {
        format!("[{address}]:{port}")
    }
}

pub fn append_ssh_known_hosts_args(args: &mut Vec<String>, known_hosts_file: Option<&Path>) {
    if let Some(known_hosts_file) = known_hosts_file {
        args.push("-o".to_owned());
        args.push(format!(
            "UserKnownHostsFile={}",
            known_hosts_file.to_string_lossy()
        ));
        args.push("-o".to_owned());
        args.push("StrictHostKeyChecking=yes".to_owned());
    }
}

async fn ensure_known_hosts_parent(path: &Path) -> Result<(), DeployError> {
    let Some(parent) = path.parent() else {
        return Err(DeployError::InvalidInput(
            "known_hosts 路径必须包含父目录".to_owned(),
        ));
    };
    fs::create_dir_all(parent).await.map_err(|err| {
        DeployError::Command(format!(
            "创建 SSH known_hosts 目录 {} 失败: {err}",
            parent.to_string_lossy()
        ))
    })
}

async fn ensure_known_hosts_file(path: &Path) -> Result<(), DeployError> {
    if fs::metadata(path).await.is_ok() {
        return Ok(());
    }
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await
        .map(|_| ())
        .or_else(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(DeployError::Command(format!(
                    "创建 SSH known_hosts 文件 {} 失败: {err}",
                    path.to_string_lossy()
                )))
            }
        })
}

async fn known_host_entry_exists(
    runner: &DynCommandRunner,
    known_hosts_file: &Path,
    lookup_key: &str,
) -> Result<bool, DeployError> {
    let result = runner
        .run(CommandSpec {
            program: "ssh-keygen".to_owned(),
            args: vec![
                "-F".to_owned(),
                lookup_key.to_owned(),
                "-f".to_owned(),
                known_hosts_file.to_string_lossy().to_string(),
            ],
            current_dir: PathBuf::from("."),
        })
        .await?;
    Ok(result.success())
}

async fn append_known_host_entry(
    runner: &DynCommandRunner,
    known_hosts_file: &Path,
    address: &str,
    port: u16,
) -> Result<(), DeployError> {
    let result = runner
        .run(CommandSpec {
            program: "ssh-keyscan".to_owned(),
            args: vec![
                "-p".to_owned(),
                port.to_string(),
                "-T".to_owned(),
                "10".to_owned(),
                "-H".to_owned(),
                address.to_owned(),
            ],
            current_dir: PathBuf::from("."),
        })
        .await?;
    if !result.success() {
        let output = result.combined_output();
        return Err(DeployError::Command(if output.trim().is_empty() {
            format!("采集 SSH 主机指纹失败: {address}:{port}")
        } else {
            format!("采集 SSH 主机指纹失败: {output}")
        }));
    }
    let entries = result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Err(DeployError::Command(format!(
            "采集 SSH 主机指纹失败: {address}:{port} 没有返回有效指纹"
        )));
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(known_hosts_file)
        .await
        .map_err(|err| {
            DeployError::Command(format!(
                "打开 SSH known_hosts 文件 {} 失败: {err}",
                known_hosts_file.to_string_lossy()
            ))
        })?;
    for entry in entries {
        file.write_all(entry.as_bytes()).await.map_err(|err| {
            DeployError::Command(format!(
                "写入 SSH known_hosts 文件 {} 失败: {err}",
                known_hosts_file.to_string_lossy()
            ))
        })?;
        file.write_all(b"\n").await.map_err(|err| {
            DeployError::Command(format!(
                "写入 SSH known_hosts 文件 {} 失败: {err}",
                known_hosts_file.to_string_lossy()
            ))
        })?;
    }
    Ok(())
}

fn render_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_owned()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn normalize_log_tail_lines(value: u16) -> u16 {
    value.clamp(50, 1000)
}

fn normalize_systemd_unit_name(value: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeployError::InvalidInput(
            "systemd unit 不能为空".to_owned(),
        ));
    }
    if !value.ends_with(".service") {
        return Err(DeployError::InvalidInput(
            "systemd unit 必须以 .service 结尾".to_owned(),
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'))
    {
        return Err(DeployError::InvalidInput(
            "systemd unit 仅支持字母、数字、短横线、下划线、点和 @".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn normalize_ssh_user(value: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeployError::InvalidInput("SSH 用户不能为空".to_owned()));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(DeployError::InvalidInput(
            "SSH 用户仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn normalize_ssh_address(value: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeployError::InvalidInput("SSH 地址不能为空".to_owned()));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(DeployError::InvalidInput(
            "SSH 地址仅支持主机名或 IPv4 地址".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn normalize_remote_absolute_path(value: &str) -> Result<String, DeployError> {
    let value = value.trim().replace('\\', "/");
    if !value.starts_with('/') {
        return Err(DeployError::InvalidInput(
            "SSH 部署路径必须是绝对路径".to_owned(),
        ));
    }
    if value.contains("//")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '@'))
        || value.split('/').any(|part| part == "." || part == "..")
    {
        return Err(DeployError::InvalidInput(
            "SSH 部署路径仅支持字母、数字、斜线、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn normalize_local_absolute_path(value: &str) -> Result<String, DeployError> {
    let value = value.trim().replace('\\', "/");
    if !value.starts_with('/') && !is_windows_absolute_path(&value) {
        return Err(DeployError::InvalidInput(
            "本机配置路径必须是绝对路径".to_owned(),
        ));
    }
    if value.contains("//")
        || !value.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '/' | '\\' | ':' | '.' | '-' | '_' | '@')
        })
        || value.split('/').any(|part| part == "." || part == "..")
    {
        return Err(DeployError::InvalidInput(
            "本机配置路径仅支持字母、数字、斜线、盘符、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn normalize_remote_health_endpoint(value: &str, label: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeployError::InvalidInput(format!(
            "{label} 健康检查地址不能为空"
        )));
    }
    if value.chars().any(char::is_whitespace)
        || !value.chars().all(|ch| ch.is_ascii_graphic())
        || value.contains('"')
        || value.contains('\'')
        || value.contains('`')
        || value.contains('$')
        || value.contains('\\')
        || value.contains(';')
        || value.contains('|')
        || value.contains('&')
        || value.contains('<')
        || value.contains('>')
    {
        return Err(DeployError::InvalidInput(format!(
            "{label} 健康检查地址包含不支持的字符"
        )));
    }
    Ok(value.to_owned())
}

fn normalize_remote_tcp_endpoint(value: &str) -> Result<(String, String), DeployError> {
    let value = normalize_remote_health_endpoint(value, "TCP")?;
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(DeployError::InvalidInput(
            "TCP 健康检查地址格式应为 host:port".to_owned(),
        ));
    };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(DeployError::InvalidInput(
            "TCP 健康检查地址格式应为 host:port".to_owned(),
        ));
    }
    Ok((host.to_owned(), port.to_owned()))
}

fn normalize_compose_service_name(value: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeployError::InvalidInput("服务名称不能为空".to_owned()));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(DeployError::InvalidInput(
            "服务名称仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct RecordingRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            let (status_code, stdout) = match spec.program.as_str() {
                "ssh-keygen" => {
                    let known_hosts_file = spec
                        .args
                        .windows(2)
                        .find(|window| window[0] == "-f")
                        .map(|window| PathBuf::from(&window[1]));
                    let exists = known_hosts_file
                        .as_ref()
                        .and_then(|path| std::fs::read_to_string(path).ok())
                        .is_some_and(|content| content.contains("ssh-ed25519"));
                    (
                        if exists { Some(0) } else { Some(1) },
                        if exists {
                            "10.0.2.11 ssh-ed25519 AAAA\n"
                        } else {
                            ""
                        },
                    )
                }
                "ssh-keyscan" => (Some(0), "10.0.2.11 ssh-ed25519 AAAA\n"),
                _ => (Some(0), "ok\n"),
            };
            self.specs.lock().expect("lock specs").push(spec);
            Ok(CommandResult {
                status_code,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn compose_config_uses_docker_compose_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner.clone());

        let output = executor
            .config(work_dir.path().to_path_buf())
            .await
            .expect("run compose config");

        assert!(output.success);
        assert_eq!(output.command, "docker compose config");
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "docker");
        assert_eq!(specs[0].args, ["compose", "config"]);
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn tokio_command_runner_times_out_slow_commands() {
        let work_dir = tempdir().expect("temp dir");
        let runner = TokioCommandRunner::new(1);

        let err = runner
            .run(CommandSpec {
                program: "powershell".to_owned(),
                args: vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 5".to_owned(),
                ],
                current_dir: work_dir.path().to_path_buf(),
            })
            .await
            .expect_err("slow command should time out");

        assert!(err.message().contains("超时"));
        assert!(err.message().contains("1 秒"));
    }

    #[tokio::test]
    async fn docker_info_uses_docker_info_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner.clone());

        let output = executor
            .docker_info(work_dir.path().to_path_buf())
            .await
            .expect("run docker info");

        assert!(output.success);
        assert_eq!(output.command, "docker info");
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "docker");
        assert_eq!(specs[0].args, ["info"]);
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn service_logs_targets_single_compose_service() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner.clone());

        let output = executor
            .service_logs(work_dir.path().to_path_buf(), "web")
            .await
            .expect("run service logs");

        assert!(output.success);
        assert_eq!(
            output.command,
            "docker compose logs --tail 200 --no-color web"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(
            specs[0].args,
            ["compose", "logs", "--tail", "200", "--no-color", "web"]
        );
    }

    #[tokio::test]
    async fn service_logs_supports_custom_tail_lines() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner.clone());

        let output = executor
            .service_logs_with_tail(work_dir.path().to_path_buf(), "web", 500)
            .await
            .expect("run service logs");

        assert!(output.success);
        assert_eq!(
            output.command,
            "docker compose logs --tail 500 --no-color web"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(
            specs[0].args,
            ["compose", "logs", "--tail", "500", "--no-color", "web"]
        );
    }

    #[tokio::test]
    async fn service_logs_rejects_invalid_service_name() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner);

        let err = executor
            .service_logs(work_dir.path().to_path_buf(), "../web")
            .await
            .expect_err("invalid service should fail");

        assert!(err.message().contains("服务名称仅支持"));
    }

    #[tokio::test]
    async fn compose_rejects_missing_work_dir() {
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner);
        let missing = PathBuf::from("definitely-missing-compose-dir");

        let err = executor
            .logs(missing)
            .await
            .expect_err("missing work dir should fail");

        assert!(err.message().contains("Compose 工作目录不存在"));
    }

    #[tokio::test]
    async fn systemd_restart_uses_systemctl_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner.clone());

        let output = executor
            .restart(work_dir.path().to_path_buf(), "orders-api.service")
            .await
            .expect("run systemctl restart");

        assert!(output.success);
        assert_eq!(output.command, "systemctl restart orders-api.service");
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "systemctl");
        assert_eq!(specs[0].args, ["restart", "orders-api.service"]);
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn systemd_daemon_reload_uses_systemctl_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner.clone());

        let output = executor
            .daemon_reload(work_dir.path().to_path_buf())
            .await
            .expect("run systemctl daemon-reload");

        assert!(output.success);
        assert_eq!(output.command, "systemctl daemon-reload");
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "systemctl");
        assert_eq!(specs[0].args, ["daemon-reload"]);
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn systemd_link_unit_uses_systemctl_link() {
        let work_dir = tempdir().expect("temp dir");
        let unit_path = work_dir.path().join("easy-deploy-worker-bin.service");
        std::fs::write(&unit_path, "[Service]\nExecStart=/bin/true\n").expect("write unit");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner.clone());

        let output = executor
            .link_unit(work_dir.path().to_path_buf(), unit_path.clone())
            .await
            .expect("run systemctl link");

        assert!(output.success);
        assert_eq!(
            output.command,
            format!("systemctl link {}", unit_path.to_string_lossy())
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "systemctl");
        assert_eq!(
            specs[0].args,
            ["link".to_owned(), unit_path.to_string_lossy().to_string()]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn systemd_make_executable_uses_chmod_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner.clone());

        let output = executor
            .make_executable(
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/orders-api/releases/v1/orders-api",
            )
            .await
            .expect("run chmod");

        assert!(output.success);
        assert_eq!(
            output.command,
            "chmod +x /opt/easy-deploy/apps/orders-api/releases/v1/orders-api"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "chmod");
        assert_eq!(
            specs[0].args,
            [
                "+x",
                "/opt/easy-deploy/apps/orders-api/releases/v1/orders-api"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn systemd_rejects_invalid_unit_name() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner);

        let err = executor
            .restart(work_dir.path().to_path_buf(), "../orders")
            .await
            .expect_err("invalid unit should fail");

        assert!(err.message().contains("systemd unit 必须以 .service 结尾"));
    }

    #[tokio::test]
    async fn systemd_logs_use_journalctl_in_work_dir() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SystemdExecutor::new(runner.clone());

        let output = executor
            .logs(work_dir.path().to_path_buf(), "orders-api.service")
            .await
            .expect("run journalctl logs");

        assert!(output.success);
        assert_eq!(
            output.command,
            "journalctl -u orders-api.service -n 200 --no-pager"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "journalctl");
        assert_eq!(
            specs[0].args,
            ["-u", "orders-api.service", "-n", "200", "--no-pager"]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_systemctl() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .restart(&target, work_dir.path().to_path_buf(), "orders-api.service")
            .await
            .expect("run remote restart");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 systemctl restart orders-api.service"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "systemctl",
                "restart",
                "orders-api.service"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_uses_managed_known_hosts_when_configured() {
        let work_dir = tempdir().expect("temp dir");
        let known_hosts_file = work_dir.path().join("ssh").join("known_hosts");
        let runner = Arc::new(RecordingRunner::default());
        let executor =
            SshExecutor::new(runner.clone()).with_known_hosts_file(known_hosts_file.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        executor
            .restart(&target, work_dir.path().to_path_buf(), "orders-api.service")
            .await
            .expect("run remote restart");

        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh-keygen");
        assert_eq!(specs[1].program, "ssh-keyscan");
        assert_eq!(specs[2].program, "ssh-keygen");
        assert_eq!(specs[3].program, "ssh");
        assert!(specs[3].args.iter().any(
            |arg| arg == &format!("UserKnownHostsFile={}", known_hosts_file.to_string_lossy())
        ));
        assert!(
            specs[3]
                .args
                .contains(&"StrictHostKeyChecking=yes".to_owned())
        );
        let known_hosts = std::fs::read_to_string(&known_hosts_file).expect("known hosts");
        assert!(known_hosts.contains("ssh-ed25519"));
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_journalctl() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .logs(&target, work_dir.path().to_path_buf(), "orders-api.service")
            .await
            .expect("run remote logs");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 journalctl -u orders-api.service -n 200 --no-pager"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "journalctl",
                "-u",
                "orders-api.service",
                "-n",
                "200",
                "--no-pager"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_links_remote_systemd_unit() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .link_unit(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/edge-bin/.easy-deploy/systemd/easy-deploy-edge-bin.service",
            )
            .await
            .expect("run remote systemctl link");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 systemctl link /opt/easy-deploy/apps/edge-bin/.easy-deploy/systemd/easy-deploy-edge-bin.service"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "systemctl",
                "link",
                "/opt/easy-deploy/apps/edge-bin/.easy-deploy/systemd/easy-deploy-edge-bin.service"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_makes_remote_binary_executable() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .make_executable(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/edge-bin/releases/v1/edge-bin",
            )
            .await
            .expect("run remote chmod");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 chmod +x /opt/easy-deploy/apps/edge-bin/releases/v1/edge-bin"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "chmod",
                "+x",
                "/opt/easy-deploy/apps/edge-bin/releases/v1/edge-bin"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_http_health_check() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .http_health_check(
                &target,
                work_dir.path().to_path_buf(),
                "http://127.0.0.1:8080/healthz",
                5,
            )
            .await
            .expect("run remote http health check");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 curl -sS -L -o /dev/null -w %{http_code} --max-time 5 --connect-timeout 5 http://127.0.0.1:8080/healthz"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "curl",
                "-sS",
                "-L",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "5",
                "--connect-timeout",
                "5",
                "http://127.0.0.1:8080/healthz"
            ]
        );
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_tcp_health_check() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .tcp_health_check(&target, work_dir.path().to_path_buf(), "127.0.0.1:19091", 5)
            .await
            .expect("run remote tcp health check");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "nc",
                "-z",
                "-w",
                "5",
                "127.0.0.1",
                "19091"
            ]
        );
    }

    #[tokio::test]
    async fn ssh_executor_copies_file_with_scp() {
        let work_dir = tempdir().expect("temp dir");
        let local_file = work_dir.path().join("orders-api");
        std::fs::write(&local_file, "binary").expect("write local file");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "prod-a", 22022).expect("valid ssh target");

        let output = executor
            .copy_file(
                &target,
                work_dir.path().to_path_buf(),
                local_file.clone(),
                "/opt/easy-deploy/apps/orders-api/current",
            )
            .await
            .expect("copy file");

        assert!(output.success);
        assert_eq!(
            output.command,
            format!(
                "scp -P 22022 {} deploy@prod-a:/opt/easy-deploy/apps/orders-api/current",
                local_file.to_string_lossy()
            )
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "scp");
        assert_eq!(specs[0].args[0], "-P");
        assert_eq!(specs[0].args[1], "22022");
        assert_eq!(
            specs[0].args[3],
            "deploy@prod-a:/opt/easy-deploy/apps/orders-api/current"
        );
    }

    #[tokio::test]
    async fn ssh_executor_uses_identity_file_for_ssh_and_scp() {
        let work_dir = tempdir().expect("temp dir");
        let local_file = work_dir.path().join("orders-api");
        std::fs::write(&local_file, "binary").expect("write local file");
        let identity_file = work_dir.path().join("id_ed25519");
        std::fs::write(&identity_file, "private").expect("write identity file");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "prod-a", 22022)
            .expect("valid ssh target")
            .with_identity_file(Some(identity_file.clone()));

        executor
            .tcp_health_check(&target, work_dir.path().to_path_buf(), "127.0.0.1:19091", 5)
            .await
            .expect("run remote tcp health check");
        executor
            .copy_file(
                &target,
                work_dir.path().to_path_buf(),
                local_file,
                "/opt/easy-deploy/apps/orders-api/current",
            )
            .await
            .expect("copy file");

        let specs = runner.specs.lock().expect("lock specs");
        let identity_arg = identity_file.to_string_lossy().to_string();
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22022",
                "-i",
                identity_arg.as_str(),
                "-o",
                "IdentitiesOnly=yes",
                "deploy@prod-a",
                "nc",
                "-z",
                "-w",
                "5",
                "127.0.0.1",
                "19091"
            ]
        );
        assert_eq!(specs[1].program, "scp");
        assert_eq!(specs[1].args[0], "-P");
        assert_eq!(specs[1].args[1], "22022");
        assert_eq!(specs[1].args[2], "-i");
        assert_eq!(specs[1].args[3], identity_arg);
        assert_eq!(specs[1].args[4], "-o");
        assert_eq!(specs[1].args[5], "IdentitiesOnly=yes");
    }

    #[tokio::test]
    async fn ssh_executor_rejects_relative_remote_path() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner);
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let err = executor
            .mkdir_all(&target, work_dir.path().to_path_buf(), "relative/path")
            .await
            .expect_err("relative remote path should fail");

        assert_eq!(err.message(), "SSH 部署路径必须是绝对路径");
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_compose_up() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .compose_up(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/orders-api",
            )
            .await
            .expect("run remote compose up");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/orders-api && docker compose up -d --remove-orphans"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "cd",
                "/opt/easy-deploy/apps/orders-api",
                "&&",
                "docker",
                "compose",
                "up",
                "-d",
                "--remove-orphans"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }

    #[tokio::test]
    async fn ssh_executor_runs_remote_compose_service_logs() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .compose_service_logs(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/orders-api",
                "web",
            )
            .await
            .expect("run remote compose service logs");

        assert!(output.success);
        assert_eq!(
            output.command,
            "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/orders-api && docker compose logs --tail 200 --no-color web"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "cd",
                "/opt/easy-deploy/apps/orders-api",
                "&&",
                "docker",
                "compose",
                "logs",
                "--tail",
                "200",
                "--no-color",
                "web"
            ]
        );
        assert_eq!(specs[0].current_dir, work_dir.path());
    }
}
