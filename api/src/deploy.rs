use std::{
    collections::VecDeque,
    future::pending,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, watch},
    time::timeout,
};

use crate::settings::DEFAULT_COMMAND_TIMEOUT_SECS;

pub type DynCommandRunner = Arc<dyn CommandRunner>;
pub type DynCommandOutputSink = Arc<dyn CommandOutputSink>;

const COMMAND_CAPTURE_HEAD_BYTES: usize = 64 * 1024;
const COMMAND_CAPTURE_TAIL_BYTES: usize = 256 * 1024;
const COMMAND_OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
const COMMAND_OUTPUT_CHANNEL_CAPACITY: usize = 16;
const COMMAND_OUTPUT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_CAPTURE_TRUNCATION_MARKER: &str = "\n...[命令输出已截断，仅保留开头与结尾]...\n";

#[derive(Debug)]
pub enum DeployError {
    InvalidInput(String),
    Command(String),
    Canceled(String),
}

impl DeployError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Command(message) | Self::Canceled(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DeployError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutputStream {
    Stdout,
    Stderr,
    System,
}

#[async_trait]
pub trait CommandOutputSink: Send + Sync {
    async fn write(&self, stream: CommandOutputStream, chunk: &[u8]) -> Result<(), DeployError>;

    async fn flush(&self) -> Result<(), DeployError> {
        Ok(())
    }
}

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

#[derive(Clone, Debug)]
pub struct CancellationSignal {
    sender: watch::Sender<bool>,
}

impl CancellationSignal {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }

    pub fn cancel(&self) -> bool {
        !self.sender.send_replace(true)
    }

    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for CancellationSignal {
    fn default() -> Self {
        Self::new()
    }
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

    async fn run_streaming(
        &self,
        spec: CommandSpec,
        output_sink: DynCommandOutputSink,
    ) -> Result<CommandResult, DeployError> {
        let result = self.run(spec).await?;
        if !result.stdout.is_empty() {
            output_sink
                .write(CommandOutputStream::Stdout, result.stdout.as_bytes())
                .await?;
        }
        if !result.stderr.is_empty() {
            output_sink
                .write(CommandOutputStream::Stderr, result.stderr.as_bytes())
                .await?;
        }
        output_sink.flush().await?;
        Ok(result)
    }

    async fn run_cancellable(
        &self,
        spec: CommandSpec,
        cancellation: CancellationSignal,
    ) -> Result<CommandResult, DeployError> {
        let command = render_command(&spec.program, &spec.args);
        if cancellation.is_cancelled() {
            return Err(DeployError::Canceled(format!(
                "命令 {command} 在启动前已取消"
            )));
        }
        tokio::select! {
            result = self.run(spec) => result,
            _ = cancellation.cancelled() => Err(DeployError::Canceled(format!("命令 {command} 已取消"))),
        }
    }

    async fn run_cancellable_streaming(
        &self,
        spec: CommandSpec,
        cancellation: CancellationSignal,
        output_sink: DynCommandOutputSink,
    ) -> Result<CommandResult, DeployError> {
        let command = render_command(&spec.program, &spec.args);
        if cancellation.is_cancelled() {
            return Err(DeployError::Canceled(format!(
                "命令 {command} 在启动前已取消"
            )));
        }
        tokio::select! {
            result = self.run_streaming(spec, output_sink) => result,
            _ = cancellation.cancelled() => Err(DeployError::Canceled(format!("命令 {command} 已取消"))),
        }
    }
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
        self.run_process(spec, None, None).await
    }

    async fn run_streaming(
        &self,
        spec: CommandSpec,
        output_sink: DynCommandOutputSink,
    ) -> Result<CommandResult, DeployError> {
        self.run_process(spec, None, Some(output_sink)).await
    }

    async fn run_cancellable(
        &self,
        spec: CommandSpec,
        cancellation: CancellationSignal,
    ) -> Result<CommandResult, DeployError> {
        self.run_process(spec, Some(&cancellation), None).await
    }

    async fn run_cancellable_streaming(
        &self,
        spec: CommandSpec,
        cancellation: CancellationSignal,
        output_sink: DynCommandOutputSink,
    ) -> Result<CommandResult, DeployError> {
        self.run_process(spec, Some(&cancellation), Some(output_sink))
            .await
    }
}

impl TokioCommandRunner {
    async fn run_process(
        &self,
        spec: CommandSpec,
        cancellation: Option<&CancellationSignal>,
        output_sink: Option<DynCommandOutputSink>,
    ) -> Result<CommandResult, DeployError> {
        let command = render_command(&spec.program, &spec.args);
        if cancellation.is_some_and(CancellationSignal::is_cancelled) {
            return Err(DeployError::Canceled(format!(
                "命令 {command} 在启动前已取消"
            )));
        }
        let mut process = Command::new(&spec.program);
        process
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.as_std_mut().process_group(0);
        }
        let mut child = process
            .spawn()
            .map_err(|err| DeployError::Command(format!("执行命令 {command} 失败: {err}")))?;
        let process_id = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            DeployError::Command(format!("执行命令 {command} 失败: 无法读取 stdout"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DeployError::Command(format!("执行命令 {command} 失败: 无法读取 stderr"))
        })?;
        let (output_sender, mut output_receiver) = mpsc::channel(COMMAND_OUTPUT_CHANNEL_CAPACITY);
        let output_tasks = vec![
            tokio::spawn(read_command_output(
                stdout,
                CommandOutputStream::Stdout,
                output_sender.clone(),
            )),
            tokio::spawn(read_command_output(
                stderr,
                CommandOutputStream::Stderr,
                output_sender.clone(),
            )),
        ];
        drop(output_sender);
        let mut wait = Box::pin(child.wait());
        let deadline = tokio::time::sleep(self.timeout);
        tokio::pin!(deadline);
        let cancellation_wait = wait_for_cancellation(cancellation);
        tokio::pin!(cancellation_wait);
        let mut output_open = true;
        let mut stdout_capture = BoundedCommandCapture::new();
        let mut stderr_capture = BoundedCommandCapture::new();
        let completion = loop {
            tokio::select! {
                biased;
                status = &mut wait => {
                    break status.map_err(|error| CommandProcessStop::OutputError(
                        DeployError::Command(format!("执行命令 {command} 失败: {error}")),
                    ));
                }
                event = output_receiver.recv(), if output_open => {
                    match event {
                        Some(event) => {
                            if let Err(error) = handle_command_output_event(
                                event,
                                output_sink.as_ref(),
                                &mut stdout_capture,
                                &mut stderr_capture,
                            ).await {
                                break Err(CommandProcessStop::OutputError(error));
                            }
                        }
                        None => output_open = false,
                    }
                }
                _ = &mut cancellation_wait => {
                    break Err(CommandProcessStop::Canceled);
                }
                _ = &mut deadline => {
                    break Err(CommandProcessStop::TimedOut);
                }
            }
        };

        let status = match completion {
            Ok(status) => status,
            Err(stop_reason) => {
                let graceful = matches!(stop_reason, CommandProcessStop::Canceled);
                terminate_process(process_id, !graceful).await;
                let grace_period = if graceful {
                    Duration::from_secs(5)
                } else {
                    Duration::from_secs(1)
                };
                if timeout(grace_period, &mut wait).await.is_err() {
                    terminate_process(process_id, true).await;
                    let _ = timeout(Duration::from_secs(5), &mut wait).await;
                }
                if let Err(error) = drain_command_output(
                    &mut output_receiver,
                    output_sink.as_ref(),
                    &mut stdout_capture,
                    &mut stderr_capture,
                )
                .await
                {
                    tracing::warn!(error = %error, "failed to drain deployment command output");
                }
                if let Err(error) = flush_command_output_sink(output_sink.as_ref()).await {
                    tracing::warn!(error = %error, "failed to flush deployment command output");
                }
                for task in &output_tasks {
                    if !task.is_finished() {
                        task.abort();
                    }
                }
                for task in output_tasks {
                    let _ = task.await;
                }
                return Err(match stop_reason {
                    CommandProcessStop::Canceled => {
                        DeployError::Canceled(format!("命令 {command} 已取消"))
                    }
                    CommandProcessStop::TimedOut => DeployError::Command(format!(
                        "执行命令 {command} 超时（{} 秒）",
                        self.timeout.as_secs()
                    )),
                    CommandProcessStop::OutputError(error) => error,
                });
            }
        };
        let drain_result = drain_command_output(
            &mut output_receiver,
            output_sink.as_ref(),
            &mut stdout_capture,
            &mut stderr_capture,
        )
        .await;
        for task in &output_tasks {
            if !task.is_finished() {
                task.abort();
            }
        }
        for task in output_tasks {
            let _ = task.await;
        }
        drain_result?;
        flush_command_output_sink(output_sink.as_ref()).await?;
        Ok(CommandResult {
            status_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout_capture.into_bytes()).to_string(),
            stderr: String::from_utf8_lossy(&stderr_capture.into_bytes()).to_string(),
        })
    }
}

async fn flush_command_output_sink(
    output_sink: Option<&DynCommandOutputSink>,
) -> Result<(), DeployError> {
    let Some(output_sink) = output_sink else {
        return Ok(());
    };
    timeout(COMMAND_OUTPUT_FLUSH_TIMEOUT, output_sink.flush())
        .await
        .map_err(|_| DeployError::Command("刷新命令输出超时".to_owned()))?
}

enum CommandProcessStop {
    Canceled,
    TimedOut,
    OutputError(DeployError),
}

enum CommandOutputEvent {
    Chunk {
        stream: CommandOutputStream,
        bytes: Vec<u8>,
    },
    ReadError {
        stream: CommandOutputStream,
        message: String,
    },
}

async fn read_command_output<R>(
    mut reader: R,
    stream: CommandOutputStream,
    sender: mpsc::Sender<CommandOutputEvent>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; COMMAND_OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => {
                if sender
                    .send(CommandOutputEvent::Chunk {
                        stream,
                        bytes: buffer[..read].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender
                    .send(CommandOutputEvent::ReadError {
                        stream,
                        message: error.to_string(),
                    })
                    .await;
                return;
            }
        }
    }
}

async fn handle_command_output_event(
    event: CommandOutputEvent,
    output_sink: Option<&DynCommandOutputSink>,
    stdout_capture: &mut BoundedCommandCapture,
    stderr_capture: &mut BoundedCommandCapture,
) -> Result<(), DeployError> {
    match event {
        CommandOutputEvent::Chunk { stream, bytes } => {
            match stream {
                CommandOutputStream::Stdout => stdout_capture.append(&bytes),
                CommandOutputStream::Stderr => stderr_capture.append(&bytes),
                CommandOutputStream::System => {}
            }
            if let Some(output_sink) = output_sink {
                output_sink.write(stream, &bytes).await?;
            }
            Ok(())
        }
        CommandOutputEvent::ReadError { stream, message } => Err(DeployError::Command(format!(
            "读取命令 {} 失败: {message}",
            match stream {
                CommandOutputStream::Stdout => "stdout",
                CommandOutputStream::Stderr => "stderr",
                CommandOutputStream::System => "output",
            }
        ))),
    }
}

async fn drain_command_output(
    receiver: &mut mpsc::Receiver<CommandOutputEvent>,
    output_sink: Option<&DynCommandOutputSink>,
    stdout_capture: &mut BoundedCommandCapture,
    stderr_capture: &mut BoundedCommandCapture,
) -> Result<(), DeployError> {
    let drain = async {
        while let Some(event) = receiver.recv().await {
            handle_command_output_event(event, output_sink, stdout_capture, stderr_capture).await?;
        }
        Ok(())
    };
    match timeout(Duration::from_secs(1), drain).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("timed out draining deployment command output");
            Ok(())
        }
    }
}

struct BoundedCommandCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped_bytes: u64,
}

impl BoundedCommandCapture {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(COMMAND_CAPTURE_HEAD_BYTES),
            tail: VecDeque::with_capacity(COMMAND_CAPTURE_TAIL_BYTES),
            dropped_bytes: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let head_bytes = (COMMAND_CAPTURE_HEAD_BYTES - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        let mut remaining = &bytes[head_bytes..];
        if self.tail.len() < COMMAND_CAPTURE_TAIL_BYTES {
            let retained = (COMMAND_CAPTURE_TAIL_BYTES - self.tail.len()).min(remaining.len());
            self.tail.extend(remaining[..retained].iter().copied());
            remaining = &remaining[retained..];
        }
        if remaining.is_empty() {
            return;
        }
        if remaining.len() >= COMMAND_CAPTURE_TAIL_BYTES {
            self.tail.clear();
            self.tail.extend(
                remaining[remaining.len() - COMMAND_CAPTURE_TAIL_BYTES..]
                    .iter()
                    .copied(),
            );
        } else {
            self.tail.drain(..remaining.len());
            self.tail.extend(remaining.iter().copied());
        }
        self.dropped_bytes = self.dropped_bytes.saturating_add(remaining.len() as u64);
    }

    fn into_bytes(self) -> Vec<u8> {
        let marker_bytes = if self.dropped_bytes > 0 {
            COMMAND_CAPTURE_TRUNCATION_MARKER.len()
        } else {
            0
        };
        let mut output = Vec::with_capacity(self.head.len() + self.tail.len() + marker_bytes);
        output.extend_from_slice(&self.head);
        if self.dropped_bytes > 0 {
            output.extend_from_slice(COMMAND_CAPTURE_TRUNCATION_MARKER.as_bytes());
        }
        output.extend(self.tail);
        output
    }
}

async fn wait_for_cancellation(cancellation: Option<&CancellationSignal>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => pending::<()>().await,
    }
}

#[cfg(unix)]
async fn terminate_process(process_id: Option<u32>, force: bool) {
    let Some(process_id) = process_id else {
        return;
    };
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    // The child starts in its own process group, so scripts and their local descendants stop together.
    let result = unsafe { libc::kill(-(process_id as i32), signal) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(process_id, force, error = %error, "failed to signal deployment command process group");
        }
    }
}

#[cfg(windows)]
async fn terminate_process(process_id: Option<u32>, _force: bool) {
    let Some(process_id) = process_id else {
        return;
    };
    let mut command = Command::new("taskkill");
    command
        .arg("/PID")
        .arg(process_id.to_string())
        .arg("/T")
        .arg("/F")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = command.status().await {
        tracing::warn!(process_id, error = %error, "failed to terminate deployment command process tree");
    }
}

#[derive(Clone)]
pub struct ComposeExecutor {
    runner: DynCommandRunner,
    output_sink: Option<DynCommandOutputSink>,
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
    output_sink: Option<DynCommandOutputSink>,
}

#[derive(Clone)]
struct CancellableCommandRunner {
    runner: DynCommandRunner,
    cancellation: CancellationSignal,
}

#[async_trait]
impl CommandRunner for CancellableCommandRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
        self.runner
            .run_cancellable(spec, self.cancellation.clone())
            .await
    }

    async fn run_streaming(
        &self,
        spec: CommandSpec,
        output_sink: DynCommandOutputSink,
    ) -> Result<CommandResult, DeployError> {
        self.runner
            .run_cancellable_streaming(spec, self.cancellation.clone(), output_sink)
            .await
    }
}

fn cancellable_runner(
    runner: &DynCommandRunner,
    cancellation: CancellationSignal,
) -> DynCommandRunner {
    Arc::new(CancellableCommandRunner {
        runner: runner.clone(),
        cancellation,
    })
}

async fn run_with_output_sink(
    runner: &DynCommandRunner,
    output_sink: Option<&DynCommandOutputSink>,
    spec: CommandSpec,
    display_command: &str,
) -> Result<CommandResult, DeployError> {
    let Some(output_sink) = output_sink else {
        return runner.run(spec).await;
    };
    output_sink
        .write(
            CommandOutputStream::System,
            format!("$ {display_command}\n").as_bytes(),
        )
        .await?;
    runner.run_streaming(spec, output_sink.clone()).await
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
        Self {
            runner,
            output_sink: None,
        }
    }

    pub fn with_cancellation(&self, cancellation: CancellationSignal) -> Self {
        Self {
            runner: cancellable_runner(&self.runner, cancellation),
            output_sink: self.output_sink.clone(),
        }
    }

    pub fn with_output_sink(&self, output_sink: DynCommandOutputSink) -> Self {
        Self {
            runner: self.runner.clone(),
            output_sink: Some(output_sink),
        }
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

    pub async fn run_script(
        &self,
        work_dir: PathBuf,
        script_relative_path: &str,
        env: &[(String, String)],
    ) -> Result<ComposeCommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "Compose 工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let script_relative_path = normalize_script_relative_path(script_relative_path)?;
        let script_path = work_dir.join(&script_relative_path);
        if !script_path.is_file() {
            return Err(DeployError::InvalidInput(format!(
                "部署脚本不存在: {}",
                script_path.to_string_lossy()
            )));
        }
        let mut args = normalized_env_args(env)?;
        args.push("sh".to_owned());
        args.push(script_relative_path);
        let command = render_command("env", &args);
        let result = run_with_output_sink(
            &self.runner,
            self.output_sink.as_ref(),
            CommandSpec {
                program: "env".to_owned(),
                args,
                current_dir: work_dir,
            },
            &command,
        )
        .await?;
        Ok(ComposeCommandOutput {
            command,
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
    }

    pub async fn run_shell_redacted(
        &self,
        work_dir: PathBuf,
        command: &str,
        display_command: &str,
    ) -> Result<ComposeCommandOutput, DeployError> {
        if !work_dir.is_dir() {
            return Err(DeployError::InvalidInput(format!(
                "Compose 工作目录不存在: {}",
                work_dir.to_string_lossy()
            )));
        }
        let result = run_with_output_sink(
            &self.runner,
            self.output_sink.as_ref(),
            CommandSpec {
                program: "sh".to_owned(),
                args: vec!["-lc".to_owned(), command.to_owned()],
                current_dir: work_dir,
            },
            display_command,
        )
        .await?;
        Ok(ComposeCommandOutput {
            command: display_command.to_owned(),
            success: result.success(),
            status_code: result.status_code,
            output: result.combined_output(),
        })
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
        let result = run_with_output_sink(
            &self.runner,
            self.output_sink.as_ref(),
            CommandSpec {
                program: "docker".to_owned(),
                args: docker_args,
                current_dir: work_dir,
            },
            &command,
        )
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
            output_sink: None,
        }
    }

    pub fn with_known_hosts_file(mut self, known_hosts_file: impl Into<PathBuf>) -> Self {
        self.known_hosts_file = Some(known_hosts_file.into());
        self
    }

    pub fn with_cancellation(&self, cancellation: CancellationSignal) -> Self {
        Self {
            runner: cancellable_runner(&self.runner, cancellation),
            known_hosts_file: self.known_hosts_file.clone(),
            output_sink: self.output_sink.clone(),
        }
    }

    pub fn with_output_sink(&self, output_sink: DynCommandOutputSink) -> Self {
        Self {
            runner: self.runner.clone(),
            known_hosts_file: self.known_hosts_file.clone(),
            output_sink: Some(output_sink),
        }
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

    pub async fn run_script(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        remote_work_dir: &str,
        script_relative_path: &str,
        env: &[(String, String)],
    ) -> Result<CommandOutput, DeployError> {
        let remote_work_dir = normalize_remote_absolute_path(remote_work_dir)?;
        let script_relative_path = normalize_script_relative_path(script_relative_path)?;
        let remote_script_path = format!("{remote_work_dir}/{script_relative_path}");
        let mut command = format!("cd {} && env", shell_quote(&remote_work_dir));
        for (key, value) in normalized_env_pairs(env)? {
            command.push(' ');
            command.push_str(&key);
            command.push('=');
            command.push_str(&shell_quote(&value));
        }
        command.push_str(" sh ");
        command.push_str(&shell_quote(&remote_script_path));
        self.run_ssh(
            target,
            local_work_dir,
            vec!["sh".to_owned(), "-lc".to_owned(), command],
        )
        .await
    }

    pub async fn run_shell_redacted(
        &self,
        target: &SshTarget,
        local_work_dir: PathBuf,
        command: &str,
        display_command: &str,
    ) -> Result<CommandOutput, DeployError> {
        let mut output = self
            .run_ssh(
                target,
                local_work_dir,
                vec!["sh".to_owned(), "-lc".to_owned(), command.to_owned()],
            )
            .await?;
        output.command = display_command.to_owned();
        Ok(output)
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
        let result = run_with_output_sink(
            &self.runner,
            self.output_sink.as_ref(),
            CommandSpec {
                program: program.to_owned(),
                args,
                current_dir: local_work_dir,
            },
            &command,
        )
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

fn normalize_script_relative_path(value: &str) -> Result<String, DeployError> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty() || value.starts_with('/') || is_windows_absolute_path(&value) {
        return Err(DeployError::InvalidInput(
            "部署脚本路径必须是相对路径".to_owned(),
        ));
    }
    if value.contains("//")
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '@'))
    {
        return Err(DeployError::InvalidInput(
            "部署脚本路径仅支持字母、数字、斜线、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(value)
}

fn normalized_env_args(env: &[(String, String)]) -> Result<Vec<String>, DeployError> {
    normalized_env_pairs(env).map(|pairs| {
        pairs
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    })
}

fn normalized_env_pairs(env: &[(String, String)]) -> Result<Vec<(String, String)>, DeployError> {
    env.iter()
        .map(|(key, value)| Ok((normalize_env_name(key)?, value.clone())))
        .collect()
}

fn normalize_env_name(value: &str) -> Result<String, DeployError> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || !value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(DeployError::InvalidInput(
            "部署脚本环境变量名仅支持大写字母、数字和下划线，且不能以数字开头".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
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
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct RecordingRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[derive(Default)]
    struct CountingOutputSink {
        bytes: AtomicUsize,
        chunks: AtomicUsize,
    }

    #[derive(Default)]
    struct CollectingOutputSink {
        content: Mutex<Vec<u8>>,
    }

    #[async_trait]
    impl CommandOutputSink for CountingOutputSink {
        async fn write(
            &self,
            _stream: CommandOutputStream,
            chunk: &[u8],
        ) -> Result<(), DeployError> {
            self.bytes.fetch_add(chunk.len(), Ordering::SeqCst);
            self.chunks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl CommandOutputSink for CollectingOutputSink {
        async fn write(
            &self,
            _stream: CommandOutputStream,
            chunk: &[u8],
        ) -> Result<(), DeployError> {
            self.content
                .lock()
                .expect("output lock")
                .extend_from_slice(chunk);
            Ok(())
        }
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

    #[derive(Default)]
    struct KeyscanFailureRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl CommandRunner for KeyscanFailureRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            let result = match spec.program.as_str() {
                "ssh-keygen" => CommandResult {
                    status_code: Some(1),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                "ssh-keyscan" => CommandResult {
                    status_code: Some(1),
                    stdout: String::new(),
                    stderr: "no route to host".to_owned(),
                },
                _ => CommandResult {
                    status_code: Some(0),
                    stdout: "ok\n".to_owned(),
                    stderr: String::new(),
                },
            };
            self.specs.lock().expect("lock specs").push(spec);
            Ok(result)
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
    async fn compose_run_shell_redacted_hides_sensitive_command_label() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let sink = Arc::new(CollectingOutputSink::default());
        let executor = ComposeExecutor::new(runner.clone()).with_output_sink(sink.clone());

        let output = executor
            .run_shell_redacted(
                work_dir.path().to_path_buf(),
                "curl 'https://bucket.oss/test?Signature=secret'",
                "download oss://bucket/test",
            )
            .await
            .expect("run shell");

        assert!(output.success);
        assert_eq!(output.command, "download oss://bucket/test");
        assert!(!output.command.contains("Signature=secret"));
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "sh");
        assert!(specs[0].args[1].contains("Signature=secret"));
        let streamed =
            String::from_utf8_lossy(&sink.content.lock().expect("output lock")).to_string();
        assert!(streamed.contains("download oss://bucket/test"));
        assert!(!streamed.contains("Signature=secret"));
    }

    #[tokio::test]
    async fn tokio_command_runner_times_out_slow_commands() {
        let work_dir = tempdir().expect("temp dir");
        let runner = TokioCommandRunner::new(1);
        let (program, args) = if cfg!(windows) {
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 5".to_owned(),
                ],
            )
        } else {
            ("sh".to_owned(), vec!["-c".to_owned(), "sleep 5".to_owned()])
        };

        let err = runner
            .run(CommandSpec {
                program,
                args,
                current_dir: work_dir.path().to_path_buf(),
            })
            .await
            .expect_err("slow command should time out");

        assert!(err.message().contains("超时"));
        assert!(err.message().contains("1 秒"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_command_runner_bounds_output_drain_after_parent_exits() {
        let work_dir = tempdir().expect("temp dir");
        let runner = TokioCommandRunner::new(10);
        let started = std::time::Instant::now();

        let result = runner
            .run(CommandSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 3 &".to_owned()],
                current_dir: work_dir.path().to_path_buf(),
            })
            .await
            .expect("parent shell exits successfully");

        assert!(result.success());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn tokio_command_runner_streams_large_output_with_bounded_capture() {
        let work_dir = tempdir().expect("temp dir");
        let runner = TokioCommandRunner::new(30);
        let sink = Arc::new(CountingOutputSink::default());
        let output_bytes = 4 * 1024 * 1024;
        let (program, args) = if cfg!(windows) {
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    format!("[Console]::Out.Write('x' * {output_bytes})"),
                ],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), format!("head -c {output_bytes} /dev/zero")],
            )
        };

        let result = runner
            .run_streaming(
                CommandSpec {
                    program,
                    args,
                    current_dir: work_dir.path().to_path_buf(),
                },
                sink.clone(),
            )
            .await
            .expect("stream large command output");

        assert_eq!(sink.bytes.load(Ordering::SeqCst), output_bytes);
        assert!(sink.chunks.load(Ordering::SeqCst) > 1);
        assert!(
            result.stdout.len()
                <= COMMAND_CAPTURE_HEAD_BYTES
                    + COMMAND_CAPTURE_TAIL_BYTES
                    + COMMAND_CAPTURE_TRUNCATION_MARKER.len()
        );
        assert!(result.stdout.contains("命令输出已截断，仅保留开头与结尾"));
    }

    #[tokio::test]
    async fn tokio_command_runner_cancels_running_process() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(TokioCommandRunner::new(30));
        let cancellation = CancellationSignal::new();
        let (program, args) = if cfg!(windows) {
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 20".to_owned(),
                ],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), "sleep 20".to_owned()],
            )
        };
        let handle = tokio::spawn({
            let runner = runner.clone();
            let cancellation = cancellation.clone();
            let current_dir = work_dir.path().to_path_buf();
            async move {
                runner
                    .run_cancellable(
                        CommandSpec {
                            program,
                            args,
                            current_dir,
                        },
                        cancellation,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cancellation.cancel());

        let error = timeout(Duration::from_secs(8), handle)
            .await
            .expect("canceled command must stop within grace period")
            .expect("command task must not panic")
            .expect_err("canceled command must fail");

        assert!(matches!(error, DeployError::Canceled(_)));
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
    async fn compose_run_script_uses_env_and_relative_script_path() {
        let work_dir = tempdir().expect("temp dir");
        let scripts_dir = work_dir.path().join(".easy-deploy").join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create scripts");
        std::fs::write(scripts_dir.join("deploy.sh"), "#!/usr/bin/env sh\n").expect("write script");
        let runner = Arc::new(RecordingRunner::default());
        let executor = ComposeExecutor::new(runner.clone());

        let output = executor
            .run_script(
                work_dir.path().to_path_buf(),
                ".easy-deploy/scripts/deploy.sh",
                &[
                    ("ED_APP_KEY".to_owned(), "orders".to_owned()),
                    ("ED_RELEASE_VERSION".to_owned(), "v1.2.3".to_owned()),
                ],
            )
            .await
            .expect("run script");

        assert!(output.success);
        assert_eq!(
            output.command,
            "env ED_APP_KEY=orders ED_RELEASE_VERSION=v1.2.3 sh .easy-deploy/scripts/deploy.sh"
        );
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "env");
        assert_eq!(
            specs[0].args,
            [
                "ED_APP_KEY=orders",
                "ED_RELEASE_VERSION=v1.2.3",
                "sh",
                ".easy-deploy/scripts/deploy.sh"
            ]
        );
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
    async fn ensure_ssh_known_host_reports_keyscan_failure_output() {
        let work_dir = tempdir().expect("temp dir");
        let known_hosts_file = work_dir.path().join("ssh").join("known_hosts");
        let runner = Arc::new(KeyscanFailureRunner::default());
        let dyn_runner: DynCommandRunner = runner.clone();

        let err = ensure_ssh_known_host(&dyn_runner, &known_hosts_file, "10.0.2.11", 22)
            .await
            .expect_err("keyscan failure should fail");

        assert!(err.message().contains("采集 SSH 主机指纹失败"));
        assert!(err.message().contains("no route to host"));
        assert!(known_hosts_file.is_file());
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].program, "ssh-keygen");
        assert_eq!(specs[1].program, "ssh-keyscan");
        assert_eq!(specs[1].args, ["-p", "22", "-T", "10", "-H", "10.0.2.11"]);
    }

    #[tokio::test]
    async fn ssh_executor_uses_managed_known_hosts_for_scp_and_remote_compose() {
        let work_dir = tempdir().expect("temp dir");
        let known_hosts_file = work_dir.path().join("ssh").join("known_hosts");
        let local_file = work_dir.path().join("orders-api");
        std::fs::write(&local_file, "binary").expect("write local file");
        let runner = Arc::new(RecordingRunner::default());
        let executor =
            SshExecutor::new(runner.clone()).with_known_hosts_file(known_hosts_file.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 2222).expect("valid ssh target");

        executor
            .copy_file(
                &target,
                work_dir.path().to_path_buf(),
                local_file,
                "/opt/easy-deploy/apps/orders-api/current",
            )
            .await
            .expect("copy file");
        executor
            .compose_up(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/orders-api",
            )
            .await
            .expect("run remote compose up");

        let specs = runner.specs.lock().expect("lock specs");
        assert!(specs.iter().any(|spec| {
            spec.program == "ssh-keygen"
                && spec
                    .args
                    .get(1)
                    .is_some_and(|arg| arg == "[10.0.2.11]:2222")
        }));
        let scp = specs
            .iter()
            .find(|spec| spec.program == "scp")
            .expect("scp command");
        let ssh = specs
            .iter()
            .rfind(|spec| spec.program == "ssh")
            .expect("ssh command");
        let known_hosts_arg = format!("UserKnownHostsFile={}", known_hosts_file.to_string_lossy());
        for spec in [scp, ssh] {
            assert!(spec.args.contains(&known_hosts_arg));
            assert!(spec.args.contains(&"StrictHostKeyChecking=yes".to_owned()));
            assert!(spec.args.contains(&"-o".to_owned()));
        }
        assert_eq!(scp.args[0], "-P");
        assert_eq!(scp.args[1], "2222");
        assert_eq!(ssh.args[0], "-p");
        assert_eq!(ssh.args[1], "2222");
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
    async fn ssh_executor_runs_remote_deploy_script_with_env() {
        let work_dir = tempdir().expect("temp dir");
        let runner = Arc::new(RecordingRunner::default());
        let executor = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.2.11", 22).expect("valid ssh target");

        let output = executor
            .run_script(
                &target,
                work_dir.path().to_path_buf(),
                "/opt/easy-deploy/apps/orders",
                ".easy-deploy/scripts/deploy.sh",
                &[
                    ("ED_APP_KEY".to_owned(), "orders".to_owned()),
                    ("ED_RELEASE_VERSION".to_owned(), "v1.2.3".to_owned()),
                ],
            )
            .await
            .expect("run remote deploy script");

        assert!(output.success);
        let specs = runner.specs.lock().expect("lock specs");
        assert_eq!(specs[0].program, "ssh");
        assert_eq!(
            specs[0].args,
            [
                "-p",
                "22",
                "deploy@10.0.2.11",
                "sh",
                "-lc",
                "cd '/opt/easy-deploy/apps/orders' && env ED_APP_KEY='orders' ED_RELEASE_VERSION='v1.2.3' sh '/opt/easy-deploy/apps/orders/.easy-deploy/scripts/deploy.sh'"
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
