use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

pub const META_DIR_NAME: &str = ".easy-deploy";
pub const COMPOSE_FILE_NAME: &str = "compose.yaml";
pub const ENV_FILE_NAME: &str = ".env";
pub const APP_META_FILE_NAME: &str = "app.yaml";
pub const DEPLOY_SCRIPT_FILE_NAME: &str = "deploy.sh";
pub const SCRIPTS_DIR_NAME: &str = "scripts";
pub const PRE_DEPLOY_SCRIPT_FILE_NAME: &str = "pre_deploy.sh";
pub const DEPLOY_STAGE_SCRIPT_FILE_NAME: &str = "deploy.sh";
pub const POST_DEPLOY_SCRIPT_FILE_NAME: &str = "post_deploy.sh";
pub const SWITCH_TRAFFIC_SCRIPT_FILE_NAME: &str = "switch_traffic.sh";
pub const CLEANUP_SCRIPT_FILE_NAME: &str = "cleanup.sh";
pub const RELEASES_DIR_NAME: &str = "releases";
pub const CURRENT_RELEASE_FILE_NAME: &str = "current";
pub const SYSTEMD_DIR_NAME: &str = "systemd";
pub const RELEASE_META_FILE_NAME: &str = "release.yaml";

#[derive(Clone)]
pub struct RuntimeFs {
    data_dir: PathBuf,
}

#[derive(Debug)]
pub enum RuntimeFsError {
    InvalidInput(String),
    Io(String),
}

impl RuntimeFsError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Io(message) => message,
        }
    }
}

impl std::fmt::Display for RuntimeFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RuntimeFsError {}

#[derive(Clone, Debug)]
pub struct AppRuntimeConfig {
    pub app_key: String,
    pub app_id: i64,
    pub name: String,
    pub description: String,
    pub environment: String,
    pub app_type: String,
    pub deploy_mode: String,
    pub deploy_strategy: String,
    pub deploy_work_dir: String,
    pub target_nodes: Vec<TargetNodeMetadata>,
    pub compose_content: String,
    pub env_content: String,
    pub deploy_scripts: DeployScriptSet,
    pub binary: Option<BinaryRuntimeMetadata>,
}

#[derive(Clone, Debug)]
pub struct TargetNodeMetadata {
    pub node_key: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct BinaryRuntimeMetadata {
    pub service_name: String,
    pub artifact_version: String,
    pub artifact_path: String,
    pub exec_args: String,
    pub working_dir: String,
    pub service_user: String,
    pub unit_name: String,
    pub release_strategy: String,
    pub active_slot: String,
    pub base_port: i64,
    pub standby_port: i64,
    pub proxy_enabled: bool,
    pub proxy_kind: String,
    pub proxy_domain: String,
    pub proxy_config_path: String,
    pub env_content: String,
}

#[derive(Clone, Debug)]
pub struct BinaryRuntimeConfig {
    pub app_key: String,
    pub app_id: i64,
    pub name: String,
    pub service_name: String,
    pub artifact_version: String,
    pub artifact_path: String,
    pub exec_args: String,
    pub working_dir: String,
    pub service_user: String,
    pub unit_name: String,
    pub release_strategy: String,
    pub active_slot: String,
    pub base_port: i64,
    pub standby_port: i64,
    pub proxy_enabled: bool,
    pub proxy_kind: String,
    pub proxy_domain: String,
    pub proxy_config_path: String,
    pub env_content: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeployScriptSet {
    #[serde(default)]
    pub pre_deploy: String,
    #[serde(default)]
    pub deploy: String,
    #[serde(default)]
    pub post_deploy: String,
    #[serde(default)]
    pub switch_traffic: String,
    #[serde(default)]
    pub cleanup: String,
}

#[derive(Clone, Debug)]
pub struct ReleaseRuntimeMetadata {
    pub app_key: String,
    pub app_id: i64,
    pub app_name: String,
    pub release_version: String,
    pub version_code: i64,
    pub package_name: String,
    pub package_path: String,
    pub extract_dir: String,
    pub checksum_sha256: String,
    pub size_bytes: u64,
    pub published_at: String,
    pub received_at: String,
    pub source: String,
    pub config_snapshot_id: Option<i64>,
    pub config_revision_no: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct AppRuntimeWriteResult {
    pub root_dir: PathBuf,
    pub metadata_content: String,
}

#[derive(Clone, Debug)]
pub struct AppRuntimeFiles {
    pub root_dir: PathBuf,
    pub compose_content: String,
    pub env_content: String,
    pub deploy_scripts: DeployScriptSet,
    pub metadata_content: String,
}

#[derive(Clone, Debug, Default)]
pub struct BinaryRuntimeFiles {
    pub unit_path: PathBuf,
    pub env_path: PathBuf,
    pub blue_unit_path: PathBuf,
    pub blue_env_path: PathBuf,
    pub green_unit_path: PathBuf,
    pub green_env_path: PathBuf,
    pub release_path: PathBuf,
    pub current_path: PathBuf,
    pub unit_content: String,
    pub env_content: String,
    pub blue_unit_content: String,
    pub blue_env_content: String,
    pub green_unit_content: String,
    pub green_env_content: String,
    pub release_content: String,
    pub current_content: String,
}

#[derive(Clone, Debug)]
pub struct BinaryRuntimeWriteResult {
    pub files: BinaryRuntimeFiles,
}

#[derive(Clone, Debug)]
pub struct ReleaseRuntimeWriteResult {
    pub release_dir: PathBuf,
    pub release_file: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct CurrentReleaseWriteResult {
    pub current_file: PathBuf,
    pub content: String,
}

impl RuntimeFs {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn app_root(&self, app_key: &str) -> Result<PathBuf, RuntimeFsError> {
        validate_key(app_key)?;
        Ok(self.data_dir.join("apps").join(app_key))
    }

    pub async fn save_app_config(
        &self,
        config: AppRuntimeConfig,
    ) -> Result<AppRuntimeWriteResult, RuntimeFsError> {
        validate_key(&config.app_key)?;
        let root_dir = self.app_root(&config.app_key)?;
        let meta_dir = root_dir.join(META_DIR_NAME);
        fs::create_dir_all(&meta_dir)
            .await
            .map_err(|err| io_error("创建应用配置目录", &root_dir, err))?;

        write_optional_file(root_dir.join(COMPOSE_FILE_NAME), &config.compose_content).await?;
        write_file(root_dir.join(ENV_FILE_NAME), &config.env_content).await?;
        write_optional_file(
            root_dir.join(META_DIR_NAME).join(DEPLOY_SCRIPT_FILE_NAME),
            "",
        )
        .await?;
        write_deploy_scripts(&meta_dir, &config.deploy_scripts).await?;

        let metadata_content = render_app_metadata(&config, &root_dir);
        write_file(meta_dir.join(APP_META_FILE_NAME), &metadata_content).await?;

        Ok(AppRuntimeWriteResult {
            root_dir,
            metadata_content,
        })
    }

    pub async fn load_app_config(&self, app_key: &str) -> Result<AppRuntimeFiles, RuntimeFsError> {
        let root_dir = self.app_root(app_key)?;
        let compose_content = read_optional_file(root_dir.join(COMPOSE_FILE_NAME)).await?;
        let env_content = read_optional_file(root_dir.join(ENV_FILE_NAME)).await?;
        let metadata_content =
            read_optional_file(root_dir.join(META_DIR_NAME).join(APP_META_FILE_NAME)).await?;
        let deploy_scripts = read_deploy_scripts(&root_dir.join(META_DIR_NAME)).await?;
        Ok(AppRuntimeFiles {
            root_dir,
            compose_content,
            env_content,
            deploy_scripts,
            metadata_content,
        })
    }

    pub async fn save_app_runtime_files(
        &self,
        app_key: &str,
        compose_content: &str,
        env_content: &str,
        metadata_content: &str,
    ) -> Result<(), RuntimeFsError> {
        self.save_app_runtime_files_with_scripts(
            app_key,
            compose_content,
            env_content,
            metadata_content,
            &DeployScriptSet::default(),
        )
        .await
    }

    pub async fn save_app_runtime_files_with_scripts(
        &self,
        app_key: &str,
        compose_content: &str,
        env_content: &str,
        metadata_content: &str,
        deploy_scripts: &DeployScriptSet,
    ) -> Result<(), RuntimeFsError> {
        let root_dir = self.app_root(app_key)?;
        let meta_dir = root_dir.join(META_DIR_NAME);
        fs::create_dir_all(&meta_dir)
            .await
            .map_err(|err| io_error("创建应用配置目录", &root_dir, err))?;
        write_optional_file(root_dir.join(COMPOSE_FILE_NAME), compose_content).await?;
        write_file(root_dir.join(ENV_FILE_NAME), env_content).await?;
        write_deploy_scripts(&meta_dir, deploy_scripts).await?;
        write_file(meta_dir.join(APP_META_FILE_NAME), metadata_content).await?;
        Ok(())
    }

    pub async fn save_binary_runtime_files(
        &self,
        config: BinaryRuntimeConfig,
    ) -> Result<BinaryRuntimeWriteResult, RuntimeFsError> {
        validate_key(&config.app_key)?;
        validate_release_id(&config.artifact_version)?;
        validate_unit_file_name(&config.unit_name)?;
        let root_dir = self.app_root(&config.app_key)?;
        let paths = binary_runtime_paths(&root_dir, &config.unit_name, &config.artifact_version);
        fs::create_dir_all(&paths.release_dir)
            .await
            .map_err(|err| io_error("创建二进制发布目录", &root_dir, err))?;
        fs::create_dir_all(&paths.systemd_dir)
            .await
            .map_err(|err| io_error("创建 systemd 配置目录", &root_dir, err))?;

        let env_content = ensure_trailing_newline(&config.env_content);
        let release_content = render_binary_release_metadata(&config, &paths);
        let unit_content = render_systemd_unit(&config, &paths.env_relative);
        let blue_unit_content =
            render_blue_green_systemd_unit(&config, "blue", &paths.blue_env_relative);
        let green_unit_content =
            render_blue_green_systemd_unit(&config, "green", &paths.green_env_relative);
        let current_content = render_current_binary_release_pointer(&config, &paths);

        write_file(paths.unit_path.clone(), &unit_content).await?;
        write_file(paths.env_path.clone(), &env_content).await?;
        write_file(paths.blue_unit_path.clone(), &blue_unit_content).await?;
        write_file(paths.blue_env_path.clone(), &env_content).await?;
        write_file(paths.green_unit_path.clone(), &green_unit_content).await?;
        write_file(paths.green_env_path.clone(), &env_content).await?;
        write_file(paths.release_path.clone(), &release_content).await?;
        write_file(paths.current_path.clone(), &current_content).await?;

        Ok(BinaryRuntimeWriteResult {
            files: BinaryRuntimeFiles {
                unit_path: paths.unit_path,
                env_path: paths.env_path,
                blue_unit_path: paths.blue_unit_path,
                blue_env_path: paths.blue_env_path,
                green_unit_path: paths.green_unit_path,
                green_env_path: paths.green_env_path,
                release_path: paths.release_path,
                current_path: paths.current_path,
                unit_content,
                env_content: env_content.clone(),
                blue_unit_content,
                blue_env_content: env_content.clone(),
                green_unit_content,
                green_env_content: env_content.clone(),
                release_content,
                current_content,
            },
        })
    }

    pub async fn save_binary_release_file(
        &self,
        app_key: &str,
        artifact_version: &str,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, RuntimeFsError> {
        self.save_release_package_file(app_key, artifact_version, file_name, bytes)
            .await
    }

    pub async fn save_release_package_file(
        &self,
        app_key: &str,
        release_version: &str,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, RuntimeFsError> {
        validate_key(app_key)?;
        validate_release_id(release_version)?;
        let file_name = sanitize_file_name(file_name)?;
        let root_dir = self.app_root(app_key)?;
        let release_dir = root_dir.join(RELEASES_DIR_NAME).join(release_version);
        fs::create_dir_all(&release_dir)
            .await
            .map_err(|err| io_error("创建发布版本目录", &release_dir, err))?;
        let artifact_path = release_dir.join(file_name);
        write_file(artifact_path.clone(), bytes).await?;
        Ok(artifact_path)
    }

    pub async fn save_release_runtime_metadata(
        &self,
        metadata: ReleaseRuntimeMetadata,
    ) -> Result<ReleaseRuntimeWriteResult, RuntimeFsError> {
        validate_key(&metadata.app_key)?;
        validate_release_id(&metadata.release_version)?;
        let root_dir = self.app_root(&metadata.app_key)?;
        let release_dir = root_dir
            .join(RELEASES_DIR_NAME)
            .join(&metadata.release_version);
        fs::create_dir_all(&release_dir)
            .await
            .map_err(|err| io_error("创建发布版本目录", &release_dir, err))?;
        let release_file = release_dir.join(RELEASE_META_FILE_NAME);
        let content = render_release_metadata(&metadata);
        write_file(release_file.clone(), &content).await?;
        Ok(ReleaseRuntimeWriteResult {
            release_dir,
            release_file,
            content,
        })
    }

    pub async fn mark_current_release(
        &self,
        app_key: &str,
        release_version: &str,
    ) -> Result<CurrentReleaseWriteResult, RuntimeFsError> {
        validate_key(app_key)?;
        validate_release_id(release_version)?;
        let root_dir = self.app_root(app_key)?;
        let current_file = root_dir.join(CURRENT_RELEASE_FILE_NAME);
        let content = render_current_release_pointer(app_key, release_version);
        write_file(current_file.clone(), &content).await?;
        Ok(CurrentReleaseWriteResult {
            current_file,
            content,
        })
    }

    pub async fn load_binary_runtime_files(
        &self,
        app_key: &str,
        unit_name: &str,
        artifact_version: &str,
    ) -> Result<BinaryRuntimeFiles, RuntimeFsError> {
        if unit_name.trim().is_empty() || artifact_version.trim().is_empty() {
            return Ok(BinaryRuntimeFiles::default());
        }
        validate_key(app_key)?;
        validate_release_id(artifact_version)?;
        validate_unit_file_name(unit_name)?;
        let root_dir = self.app_root(app_key)?;
        let paths = binary_runtime_paths(&root_dir, unit_name, artifact_version);
        Ok(BinaryRuntimeFiles {
            unit_content: read_optional_file(paths.unit_path.clone()).await?,
            env_content: read_optional_file(paths.env_path.clone()).await?,
            blue_unit_content: read_optional_file(paths.blue_unit_path.clone()).await?,
            blue_env_content: read_optional_file(paths.blue_env_path.clone()).await?,
            green_unit_content: read_optional_file(paths.green_unit_path.clone()).await?,
            green_env_content: read_optional_file(paths.green_env_path.clone()).await?,
            release_content: read_optional_file(paths.release_path.clone()).await?,
            current_content: read_optional_file(paths.current_path.clone()).await?,
            unit_path: paths.unit_path,
            env_path: paths.env_path,
            blue_unit_path: paths.blue_unit_path,
            blue_env_path: paths.blue_env_path,
            green_unit_path: paths.green_unit_path,
            green_env_path: paths.green_env_path,
            release_path: paths.release_path,
            current_path: paths.current_path,
        })
    }
}

struct BinaryRuntimePaths {
    systemd_dir: PathBuf,
    release_dir: PathBuf,
    unit_path: PathBuf,
    env_path: PathBuf,
    blue_unit_path: PathBuf,
    blue_env_path: PathBuf,
    green_unit_path: PathBuf,
    green_env_path: PathBuf,
    release_path: PathBuf,
    current_path: PathBuf,
    unit_relative: String,
    env_relative: String,
    blue_unit_relative: String,
    blue_env_relative: String,
    green_unit_relative: String,
    green_env_relative: String,
    release_relative: String,
    current_relative: String,
}

async fn write_file(path: PathBuf, content: impl AsRef<[u8]>) -> Result<(), RuntimeFsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|err| io_error("创建父目录", parent, err))?;
    }
    fs::write(&path, content)
        .await
        .map_err(|err| io_error("写入文件", &path, err))
}

async fn write_optional_file(path: PathBuf, content: &str) -> Result<(), RuntimeFsError> {
    if content.trim().is_empty() {
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_error("删除空配置文件", &path, err)),
        }
    } else {
        write_file(path, content).await
    }
}

async fn read_optional_file(path: PathBuf) -> Result<String, RuntimeFsError> {
    match fs::read_to_string(&path).await {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(io_error("读取文件", &path, err)),
    }
}

async fn write_deploy_scripts(
    meta_dir: &Path,
    scripts: &DeployScriptSet,
) -> Result<(), RuntimeFsError> {
    let scripts_dir = meta_dir.join(SCRIPTS_DIR_NAME);
    fs::create_dir_all(&scripts_dir)
        .await
        .map_err(|err| io_error("创建脚本目录", &scripts_dir, err))?;
    write_optional_file(
        scripts_dir.join(PRE_DEPLOY_SCRIPT_FILE_NAME),
        &scripts.pre_deploy,
    )
    .await?;
    write_optional_file(
        scripts_dir.join(DEPLOY_STAGE_SCRIPT_FILE_NAME),
        &scripts.deploy,
    )
    .await?;
    write_optional_file(
        scripts_dir.join(POST_DEPLOY_SCRIPT_FILE_NAME),
        &scripts.post_deploy,
    )
    .await?;
    write_optional_file(
        scripts_dir.join(SWITCH_TRAFFIC_SCRIPT_FILE_NAME),
        &scripts.switch_traffic,
    )
    .await?;
    write_optional_file(scripts_dir.join(CLEANUP_SCRIPT_FILE_NAME), &scripts.cleanup).await?;
    Ok(())
}

async fn read_deploy_scripts(meta_dir: &Path) -> Result<DeployScriptSet, RuntimeFsError> {
    let scripts_dir = meta_dir.join(SCRIPTS_DIR_NAME);
    Ok(DeployScriptSet {
        pre_deploy: read_optional_file(scripts_dir.join(PRE_DEPLOY_SCRIPT_FILE_NAME)).await?,
        deploy: read_optional_file(scripts_dir.join(DEPLOY_STAGE_SCRIPT_FILE_NAME)).await?,
        post_deploy: read_optional_file(scripts_dir.join(POST_DEPLOY_SCRIPT_FILE_NAME)).await?,
        switch_traffic: read_optional_file(scripts_dir.join(SWITCH_TRAFFIC_SCRIPT_FILE_NAME))
            .await?,
        cleanup: read_optional_file(scripts_dir.join(CLEANUP_SCRIPT_FILE_NAME)).await?,
    })
}

fn render_app_metadata(config: &AppRuntimeConfig, root_dir: &Path) -> String {
    let mut output = String::new();
    output.push_str("app_id: ");
    output.push_str(&config.app_id.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "app_key", &config.app_key);
    push_yaml_string(&mut output, "name", &config.name);
    push_yaml_string(&mut output, "description", &config.description);
    push_yaml_string(&mut output, "environment", &config.environment);
    push_yaml_string(&mut output, "app_type", &config.app_type);
    push_yaml_string(&mut output, "deploy_mode", &config.deploy_mode);
    push_yaml_string(&mut output, "deploy_strategy", &config.deploy_strategy);
    push_yaml_string(&mut output, "deploy_work_dir", &config.deploy_work_dir);
    push_yaml_string(&mut output, "runtime_root", &root_dir.to_string_lossy());
    output.push_str("deploy_scripts:\n");
    push_indented_yaml_string(
        &mut output,
        "pre_deploy",
        script_metadata_path(PRE_DEPLOY_SCRIPT_FILE_NAME),
        2,
    );
    push_indented_yaml_string(
        &mut output,
        "deploy",
        script_metadata_path(DEPLOY_STAGE_SCRIPT_FILE_NAME),
        2,
    );
    push_indented_yaml_string(
        &mut output,
        "post_deploy",
        script_metadata_path(POST_DEPLOY_SCRIPT_FILE_NAME),
        2,
    );
    push_indented_yaml_string(
        &mut output,
        "switch_traffic",
        script_metadata_path(SWITCH_TRAFFIC_SCRIPT_FILE_NAME),
        2,
    );
    push_indented_yaml_string(
        &mut output,
        "cleanup",
        script_metadata_path(CLEANUP_SCRIPT_FILE_NAME),
        2,
    );
    output.push_str("target_nodes:\n");
    for node in &config.target_nodes {
        output.push_str("  - ");
        push_inline_yaml_pair(&mut output, "node_key", &node.node_key);
        output.push('\n');
        output.push_str("    ");
        push_inline_yaml_pair(&mut output, "name", &node.name);
        output.push('\n');
    }
    if let Some(binary) = &config.binary {
        let paths = binary_runtime_relative_paths(&binary.unit_name, &binary.artifact_version);
        output.push_str("binary:\n");
        push_indented_yaml_string(&mut output, "service_name", &binary.service_name, 2);
        push_indented_yaml_string(&mut output, "artifact_version", &binary.artifact_version, 2);
        push_indented_yaml_string(&mut output, "artifact_path", &binary.artifact_path, 2);
        push_indented_yaml_string(&mut output, "exec_args", &binary.exec_args, 2);
        push_indented_yaml_string(&mut output, "working_dir", &binary.working_dir, 2);
        push_indented_yaml_string(&mut output, "service_user", &binary.service_user, 2);
        push_indented_yaml_string(&mut output, "unit_name", &binary.unit_name, 2);
        push_indented_yaml_string(&mut output, "release_strategy", &binary.release_strategy, 2);
        push_indented_yaml_string(&mut output, "active_slot", &binary.active_slot, 2);
        output.push_str("  base_port: ");
        output.push_str(&binary.base_port.to_string());
        output.push('\n');
        output.push_str("  standby_port: ");
        output.push_str(&binary.standby_port.to_string());
        output.push('\n');
        output.push_str("  proxy_enabled: ");
        output.push_str(if binary.proxy_enabled {
            "true"
        } else {
            "false"
        });
        output.push('\n');
        push_indented_yaml_string(&mut output, "proxy_kind", &binary.proxy_kind, 2);
        push_indented_yaml_string(&mut output, "proxy_domain", &binary.proxy_domain, 2);
        push_indented_yaml_string(
            &mut output,
            "proxy_config_path",
            &binary.proxy_config_path,
            2,
        );
        push_indented_yaml_string(&mut output, "unit_file", &paths.unit_relative, 2);
        push_indented_yaml_string(&mut output, "env_file", &paths.env_relative, 2);
        push_indented_yaml_string(&mut output, "release_file", &paths.release_relative, 2);
        push_indented_yaml_string(
            &mut output,
            "current_release_file",
            &paths.current_relative,
            2,
        );
    }
    output
}

fn script_metadata_path(file_name: &str) -> &str {
    match file_name {
        PRE_DEPLOY_SCRIPT_FILE_NAME => ".easy-deploy/scripts/pre_deploy.sh",
        DEPLOY_STAGE_SCRIPT_FILE_NAME => ".easy-deploy/scripts/deploy.sh",
        POST_DEPLOY_SCRIPT_FILE_NAME => ".easy-deploy/scripts/post_deploy.sh",
        SWITCH_TRAFFIC_SCRIPT_FILE_NAME => ".easy-deploy/scripts/switch_traffic.sh",
        CLEANUP_SCRIPT_FILE_NAME => ".easy-deploy/scripts/cleanup.sh",
        _ => "",
    }
}

fn push_yaml_string(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": ");
    output.push('"');
    output.push_str(&escape_yaml_string(value));
    output.push_str("\"\n");
}

fn push_indented_yaml_string(output: &mut String, key: &str, value: &str, indent: usize) {
    output.push_str(&" ".repeat(indent));
    push_yaml_string(output, key, value);
}

fn push_inline_yaml_pair(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": ");
    output.push('"');
    output.push_str(&escape_yaml_string(value));
    output.push('"');
}

fn escape_yaml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_systemd_unit(config: &BinaryRuntimeConfig, env_relative: &str) -> String {
    render_systemd_unit_content(
        config,
        "",
        &config.unit_name,
        env_relative,
        config.artifact_path.clone(),
        config.exec_args.clone(),
        0,
    )
}

fn render_blue_green_systemd_unit(
    config: &BinaryRuntimeConfig,
    slot: &str,
    env_relative: &str,
) -> String {
    let slot_port = match slot {
        "blue" => config.base_port,
        "green" => config.standby_port,
        _ => 0,
    };
    let template_port = if config.base_port > 0 {
        config.base_port
    } else {
        slot_port
    };
    let (exec_start, exec_args) =
        slot_exec_start(&config.artifact_path, &config.exec_args, template_port);
    render_systemd_unit_content(
        config,
        slot,
        &blue_green_unit_name(&config.unit_name, slot),
        env_relative,
        exec_start,
        exec_args,
        slot_port,
    )
}

fn render_systemd_unit_content(
    config: &BinaryRuntimeConfig,
    slot: &str,
    _unit_name: &str,
    env_relative: &str,
    executable_path: String,
    exec_args: String,
    port: i64,
) -> String {
    let env_file = target_path(&config.working_dir, env_relative);
    let working_dir = config.working_dir.replace('\\', "/");
    let exec_start = if exec_args.trim().is_empty() {
        executable_path
    } else {
        format!("{} {}", executable_path, exec_args.trim())
    };
    let description_slot = if slot.is_empty() {
        String::new()
    } else {
        format!(" {slot}")
    };
    let port_env = if port > 0 {
        format!("Environment=PORT={port}\n")
    } else {
        String::new()
    };
    format!(
        "[Unit]\nDescription=Easy Deploy {} ({}){}\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory={}\nEnvironmentFile=-{}\n{}User={}\nExecStart={}\nRestart=always\nRestartSec=5\nKillSignal=SIGTERM\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n",
        config.name,
        config.app_key,
        description_slot,
        working_dir,
        env_file,
        port_env,
        config.service_user,
        exec_start,
    )
}

fn render_binary_release_metadata(
    config: &BinaryRuntimeConfig,
    paths: &BinaryRuntimePaths,
) -> String {
    let mut output = String::new();
    output.push_str("app_id: ");
    output.push_str(&config.app_id.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "app_key", &config.app_key);
    push_yaml_string(&mut output, "service_name", &config.service_name);
    push_yaml_string(&mut output, "artifact_version", &config.artifact_version);
    push_yaml_string(&mut output, "artifact_path", &config.artifact_path);
    push_yaml_string(&mut output, "exec_args", &config.exec_args);
    push_yaml_string(&mut output, "working_dir", &config.working_dir);
    push_yaml_string(&mut output, "service_user", &config.service_user);
    push_yaml_string(&mut output, "unit_name", &config.unit_name);
    push_yaml_string(&mut output, "release_strategy", &config.release_strategy);
    push_yaml_string(&mut output, "active_slot", &config.active_slot);
    output.push_str("base_port: ");
    output.push_str(&config.base_port.to_string());
    output.push('\n');
    output.push_str("standby_port: ");
    output.push_str(&config.standby_port.to_string());
    output.push('\n');
    output.push_str("proxy_enabled: ");
    output.push_str(if config.proxy_enabled {
        "true"
    } else {
        "false"
    });
    output.push('\n');
    push_yaml_string(&mut output, "proxy_kind", &config.proxy_kind);
    push_yaml_string(&mut output, "proxy_domain", &config.proxy_domain);
    push_yaml_string(&mut output, "proxy_config_path", &config.proxy_config_path);
    push_yaml_string(&mut output, "unit_file", &paths.unit_relative);
    push_yaml_string(&mut output, "env_file", &paths.env_relative);
    push_yaml_string(&mut output, "release_file", &paths.release_relative);
    push_yaml_string(&mut output, "current_release_file", &paths.current_relative);
    output
}

fn render_current_binary_release_pointer(
    config: &BinaryRuntimeConfig,
    paths: &BinaryRuntimePaths,
) -> String {
    let mut output = String::new();
    push_yaml_string(&mut output, "artifact_version", &config.artifact_version);
    push_yaml_string(&mut output, "release_file", &paths.release_relative);
    output
}

fn render_release_metadata(metadata: &ReleaseRuntimeMetadata) -> String {
    let mut output = String::new();
    output.push_str("app_id: ");
    output.push_str(&metadata.app_id.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "app_key", &metadata.app_key);
    push_yaml_string(&mut output, "app_name", &metadata.app_name);
    push_yaml_string(&mut output, "release_version", &metadata.release_version);
    output.push_str("version_code: ");
    output.push_str(&metadata.version_code.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "package_name", &metadata.package_name);
    push_yaml_string(&mut output, "package_path", &metadata.package_path);
    push_yaml_string(&mut output, "extract_dir", &metadata.extract_dir);
    push_yaml_string(&mut output, "checksum_sha256", &metadata.checksum_sha256);
    output.push_str("size_bytes: ");
    output.push_str(&metadata.size_bytes.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "published_at", &metadata.published_at);
    push_yaml_string(&mut output, "received_at", &metadata.received_at);
    push_yaml_string(&mut output, "source", &metadata.source);
    if let Some(config_snapshot_id) = metadata.config_snapshot_id {
        output.push_str("config_snapshot_id: ");
        output.push_str(&config_snapshot_id.to_string());
        output.push('\n');
    }
    if let Some(config_revision_no) = metadata.config_revision_no {
        output.push_str("config_revision_no: ");
        output.push_str(&config_revision_no.to_string());
        output.push('\n');
    }
    push_yaml_string(
        &mut output,
        "release_file",
        &format!(
            "{RELEASES_DIR_NAME}/{}/{}",
            metadata.release_version, RELEASE_META_FILE_NAME
        ),
    );
    output
}

fn render_current_release_pointer(app_key: &str, release_version: &str) -> String {
    let mut output = String::new();
    push_yaml_string(&mut output, "app_key", app_key);
    push_yaml_string(&mut output, "release_version", release_version);
    push_yaml_string(
        &mut output,
        "release_file",
        &format!("{RELEASES_DIR_NAME}/{release_version}/{RELEASE_META_FILE_NAME}"),
    );
    output
}

fn binary_runtime_paths(
    root_dir: &Path,
    unit_name: &str,
    artifact_version: &str,
) -> BinaryRuntimePaths {
    let relative = binary_runtime_relative_paths(unit_name, artifact_version);
    let systemd_dir = root_dir.join(META_DIR_NAME).join(SYSTEMD_DIR_NAME);
    let release_dir = root_dir.join(RELEASES_DIR_NAME).join(artifact_version);
    BinaryRuntimePaths {
        unit_path: root_dir.join(&relative.unit_relative),
        env_path: root_dir.join(&relative.env_relative),
        blue_unit_path: root_dir.join(&relative.blue_unit_relative),
        blue_env_path: root_dir.join(&relative.blue_env_relative),
        green_unit_path: root_dir.join(&relative.green_unit_relative),
        green_env_path: root_dir.join(&relative.green_env_relative),
        release_path: root_dir.join(&relative.release_relative),
        current_path: root_dir.join(&relative.current_relative),
        systemd_dir,
        release_dir,
        unit_relative: relative.unit_relative,
        env_relative: relative.env_relative,
        blue_unit_relative: relative.blue_unit_relative,
        blue_env_relative: relative.blue_env_relative,
        green_unit_relative: relative.green_unit_relative,
        green_env_relative: relative.green_env_relative,
        release_relative: relative.release_relative,
        current_relative: relative.current_relative,
    }
}

fn binary_runtime_relative_paths(unit_name: &str, artifact_version: &str) -> BinaryRuntimePaths {
    let env_file_name = unit_env_file_name(unit_name);
    let blue_unit_name = blue_green_unit_name(unit_name, "blue");
    let green_unit_name = blue_green_unit_name(unit_name, "green");
    let blue_env_file_name = unit_env_file_name(&blue_unit_name);
    let green_env_file_name = unit_env_file_name(&green_unit_name);
    let unit_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{unit_name}");
    let env_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{env_file_name}");
    let blue_unit_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{blue_unit_name}");
    let blue_env_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{blue_env_file_name}");
    let green_unit_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{green_unit_name}");
    let green_env_relative = format!("{META_DIR_NAME}/{SYSTEMD_DIR_NAME}/{green_env_file_name}");
    let release_relative =
        format!("{RELEASES_DIR_NAME}/{artifact_version}/{RELEASE_META_FILE_NAME}");
    BinaryRuntimePaths {
        systemd_dir: PathBuf::new(),
        release_dir: PathBuf::new(),
        unit_path: PathBuf::new(),
        env_path: PathBuf::new(),
        blue_unit_path: PathBuf::new(),
        blue_env_path: PathBuf::new(),
        green_unit_path: PathBuf::new(),
        green_env_path: PathBuf::new(),
        release_path: PathBuf::new(),
        current_path: PathBuf::new(),
        unit_relative,
        env_relative,
        blue_unit_relative,
        blue_env_relative,
        green_unit_relative,
        green_env_relative,
        release_relative,
        current_relative: CURRENT_RELEASE_FILE_NAME.to_owned(),
    }
}

fn unit_env_file_name(unit_name: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}.env")
}

fn blue_green_unit_name(unit_name: &str, slot: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}-{slot}.service")
}

fn slot_exec_start(artifact_path: &str, exec_args: &str, port: i64) -> (String, String) {
    if port <= 0 {
        return (artifact_path.to_owned(), exec_args.to_owned());
    }
    let port_value = port.to_string();
    let normalized_path = artifact_path.replace(port_value.as_str(), "${PORT}");
    let normalized_args = replace_port_in_args(exec_args, port);
    (normalized_path, normalized_args)
}

fn replace_port_in_args(exec_args: &str, port: i64) -> String {
    let port_value = port.to_string();
    let mut previous_is_port_flag = false;
    let mut normalized = Vec::new();
    for part in exec_args.split_whitespace() {
        let next = if previous_is_port_flag && part == port_value {
            "${PORT}".to_owned()
        } else if part == port_value
            || part
                .strip_prefix("--port=")
                .is_some_and(|value| value == port_value)
        {
            part.replace(port_value.as_str(), "${PORT}")
        } else {
            part.to_owned()
        };
        previous_is_port_flag = matches!(part, "--port" | "-p");
        normalized.push(next);
    }
    normalized.join(" ")
}

fn target_path(work_dir: &str, relative_path: &str) -> String {
    let normalized_work_dir = work_dir.replace('\\', "/");
    let work_dir = normalized_work_dir.trim_end_matches('/');
    if work_dir.is_empty() {
        relative_path.to_owned()
    } else {
        format!("{work_dir}/{relative_path}")
    }
}

fn ensure_trailing_newline(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else if value.ends_with('\n') {
        value.to_owned()
    } else {
        format!("{value}\n")
    }
}

fn validate_key(value: &str) -> Result<(), RuntimeFsError> {
    if value.trim().is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(RuntimeFsError::InvalidInput(
            "运行时应用标识仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(())
}

fn validate_release_id(value: &str) -> Result<(), RuntimeFsError> {
    if value.trim().is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(RuntimeFsError::InvalidInput(
            "发布版本仅支持字母、数字、短横线、下划线和点".to_owned(),
        ));
    }
    Ok(())
}

fn validate_unit_file_name(value: &str) -> Result<(), RuntimeFsError> {
    if value.trim().is_empty()
        || !value.ends_with(".service")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'))
    {
        return Err(RuntimeFsError::InvalidInput(
            "systemd unit 文件名无效".to_owned(),
        ));
    }
    Ok(())
}

fn sanitize_file_name(value: &str) -> Result<String, RuntimeFsError> {
    let Some(file_name) = Path::new(value)
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .map(str::trim)
    else {
        return Err(RuntimeFsError::InvalidInput("版本包文件名无效".to_owned()));
    };
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || !file_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+' | '@'))
    {
        return Err(RuntimeFsError::InvalidInput(
            "版本包文件名仅支持字母、数字、短横线、下划线、点、加号和 @".to_owned(),
        ));
    }
    Ok(file_name.to_owned())
}

fn io_error(action: &str, path: &Path, err: std::io::Error) -> RuntimeFsError {
    RuntimeFsError::Io(format!("{action} {} 失败: {err}", path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn slot_exec_start_replaces_split_port_argument() {
        let (path, args) = slot_exec_start(
            "/opt/easy-deploy/worker",
            "--host 0.0.0.0 --port 8080",
            8080,
        );

        assert_eq!(path, "/opt/easy-deploy/worker");
        assert_eq!(args, "--host 0.0.0.0 --port ${PORT}");
    }

    #[test]
    fn slot_exec_start_replaces_inline_port_argument() {
        let (_, args) = slot_exec_start("/opt/easy-deploy/worker", "--port=8080", 8080);

        assert_eq!(args, "--port=${PORT}");
    }

    #[tokio::test]
    async fn save_release_runtime_metadata_writes_release_yaml() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());

        let result = runtime
            .save_release_runtime_metadata(ReleaseRuntimeMetadata {
                app_key: "orders-api-prod".to_owned(),
                app_id: 42,
                app_name: "Orders API".to_owned(),
                release_version: "v1.2.3".to_owned(),
                version_code: 1_002_003,
                package_name: "orders-api-prod_version_1_2_3.tar.gz".to_owned(),
                package_path: "/opt/apps/orders/releases/v1.2.3/package.tar.gz".to_owned(),
                extract_dir: "/opt/apps/orders/releases/v1.2.3".to_owned(),
                checksum_sha256: "abc123".to_owned(),
                size_bytes: 128,
                published_at: "2026-06-23T00:00:00.000Z".to_owned(),
                received_at: "2026-06-23T00:01:00.000Z".to_owned(),
                source: "openapi".to_owned(),
                config_snapshot_id: Some(7),
                config_revision_no: Some(3),
            })
            .await
            .expect("save release metadata");

        assert!(result.release_file.is_file());
        assert_eq!(
            result.release_dir,
            data_dir
                .path()
                .join("apps")
                .join("orders-api-prod")
                .join(RELEASES_DIR_NAME)
                .join("v1.2.3")
        );
        assert!(result.content.contains("release_version: \"v1.2.3\""));
        assert!(result.content.contains("version_code: 1002003"));
        assert!(result.content.contains("config_revision_no: 3"));
    }

    #[tokio::test]
    async fn save_app_config_persists_deploy_scripts() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());
        let scripts = DeployScriptSet {
            pre_deploy: "echo pre".to_owned(),
            deploy: "docker compose up -d".to_owned(),
            post_deploy: "echo post".to_owned(),
            switch_traffic: "echo switch".to_owned(),
            cleanup: "echo cleanup".to_owned(),
        };

        runtime
            .save_app_config(AppRuntimeConfig {
                app_key: "orders-api-prod".to_owned(),
                app_id: 42,
                name: "Orders API".to_owned(),
                description: String::new(),
                environment: "production".to_owned(),
                app_type: "compose".to_owned(),
                deploy_mode: "compose".to_owned(),
                deploy_strategy: "rolling_stop_on_failure".to_owned(),
                deploy_work_dir: "/opt/easy-deploy/apps/orders-api".to_owned(),
                target_nodes: vec![TargetNodeMetadata {
                    node_key: "node-1".to_owned(),
                    name: "node 1".to_owned(),
                }],
                compose_content: "services: {}\n".to_owned(),
                env_content: "APP_ENV=production\n".to_owned(),
                deploy_scripts: scripts.clone(),
                binary: None,
            })
            .await
            .expect("save app config");

        let loaded = runtime
            .load_app_config("orders-api-prod")
            .await
            .expect("load app config");
        assert_eq!(loaded.deploy_scripts, scripts);
        let deploy_script_path = data_dir
            .path()
            .join("apps")
            .join("orders-api-prod")
            .join(META_DIR_NAME)
            .join(SCRIPTS_DIR_NAME)
            .join(DEPLOY_STAGE_SCRIPT_FILE_NAME);
        let deploy_script = fs::read_to_string(deploy_script_path)
            .await
            .expect("read deploy script");
        assert_eq!(deploy_script, "docker compose up -d");
        assert!(loaded.metadata_content.contains("deploy_scripts:"));
        assert!(
            loaded
                .metadata_content
                .contains(".easy-deploy/scripts/deploy.sh")
        );
    }

    #[tokio::test]
    async fn mark_current_release_writes_current_pointer() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());

        let result = runtime
            .mark_current_release("orders-api-prod", "v1.2.3")
            .await
            .expect("mark current release");

        assert!(result.current_file.is_file());
        assert!(result.content.contains("app_key: \"orders-api-prod\""));
        assert!(result.content.contains("release_version: \"v1.2.3\""));
        assert!(
            result
                .content
                .contains("release_file: \"releases/v1.2.3/release.yaml\"")
        );
    }

    #[tokio::test]
    async fn save_release_package_file_sanitizes_name_and_rejects_invalid_input() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());

        let artifact_path = runtime
            .save_release_package_file(
                "orders-api-prod",
                "v1.2.3",
                "../orders-api+prod@1.tar.gz",
                b"package-bytes",
            )
            .await
            .expect("save package");

        assert_eq!(
            artifact_path,
            data_dir
                .path()
                .join("apps")
                .join("orders-api-prod")
                .join(RELEASES_DIR_NAME)
                .join("v1.2.3")
                .join("orders-api+prod@1.tar.gz")
        );
        assert_eq!(
            fs::read(&artifact_path).await.expect("read package"),
            b"package-bytes"
        );
        assert!(
            runtime
                .save_release_package_file("bad key", "v1.2.3", "pkg.tar.gz", b"")
                .await
                .is_err()
        );
        assert!(
            runtime
                .save_release_package_file("orders-api", "../v1", "pkg.tar.gz", b"")
                .await
                .is_err()
        );
        assert!(
            runtime
                .save_release_package_file("orders-api", "v1.2.3", "bad name.tar.gz", b"")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn save_app_runtime_files_removes_empty_optional_files() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());
        let scripts = DeployScriptSet {
            deploy: "docker compose up -d".to_owned(),
            ..DeployScriptSet::default()
        };

        runtime
            .save_app_runtime_files_with_scripts(
                "orders-api",
                "services: {}\n",
                "RUST_LOG=info\n",
                "app_key: \"orders-api\"\n",
                &scripts,
            )
            .await
            .expect("save runtime files");
        runtime
            .save_app_runtime_files("orders-api", "", "", "app_key: \"orders-api\"\n")
            .await
            .expect("save empty optional files");

        let loaded = runtime
            .load_app_config("orders-api")
            .await
            .expect("load runtime files");
        assert_eq!(loaded.compose_content, "");
        assert_eq!(loaded.env_content, "");
        assert_eq!(loaded.deploy_scripts, DeployScriptSet::default());
        assert!(!loaded.root_dir.join(COMPOSE_FILE_NAME).exists());
        assert!(
            !loaded
                .root_dir
                .join(META_DIR_NAME)
                .join(SCRIPTS_DIR_NAME)
                .join(DEPLOY_STAGE_SCRIPT_FILE_NAME)
                .exists()
        );
    }

    #[tokio::test]
    async fn save_binary_runtime_files_renders_and_loads_all_runtime_files() {
        let data_dir = tempdir().expect("create temp data dir");
        let runtime = RuntimeFs::new(data_dir.path());

        let result = runtime
            .save_binary_runtime_files(binary_runtime_config())
            .await
            .expect("save binary runtime");
        let files = result.files;

        assert!(files.unit_path.is_file());
        assert!(files.env_path.is_file());
        assert!(files.blue_unit_path.is_file());
        assert!(files.green_unit_path.is_file());
        assert!(
            files
                .unit_content
                .contains("ExecStart=/opt/worker/bin/server --port 8080")
        );
        assert!(files.blue_unit_content.contains("Environment=PORT=8080"));
        assert!(
            files
                .blue_unit_content
                .contains("ExecStart=/opt/worker/bin/server --port ${PORT}")
        );
        assert!(files.green_unit_content.contains("Environment=PORT=18080"));
        assert!(
            files
                .release_content
                .contains("artifact_version: \"v1.2.3\"")
        );
        assert!(
            files
                .current_content
                .contains("artifact_version: \"v1.2.3\"")
        );

        let loaded = runtime
            .load_binary_runtime_files("worker-bin", "easy-deploy-worker-bin.service", "v1.2.3")
            .await
            .expect("load binary runtime");
        assert_eq!(loaded.unit_content, files.unit_content);
        assert_eq!(loaded.env_content, "RUST_LOG=info\n");
        assert_eq!(loaded.release_content, files.release_content);

        let empty = runtime
            .load_binary_runtime_files("worker-bin", "", "v1.2.3")
            .await
            .expect("empty binary runtime");
        assert_eq!(empty.unit_content, "");
        assert_eq!(empty.release_content, "");
    }

    #[test]
    fn validators_and_path_helpers_cover_boundaries() {
        assert!(validate_key("orders-api_1").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("orders api").is_err());
        assert!(validate_release_id("v1.2.3").is_ok());
        assert!(validate_release_id("../v1").is_err());
        assert!(validate_unit_file_name("easy-deploy-worker@blue.service").is_ok());
        assert!(validate_unit_file_name("worker.timer").is_err());
        assert_eq!(
            sanitize_file_name("../orders+prod@1.tar.gz").expect("sanitize"),
            "orders+prod@1.tar.gz"
        );
        assert!(sanitize_file_name("bad name.tar.gz").is_err());

        assert_eq!(
            script_metadata_path(PRE_DEPLOY_SCRIPT_FILE_NAME),
            ".easy-deploy/scripts/pre_deploy.sh"
        );
        assert_eq!(script_metadata_path("unknown.sh"), "");
        assert_eq!(
            target_path(r"C:\apps\worker\", ".easy-deploy/systemd/worker.service"),
            "C:/apps/worker/.easy-deploy/systemd/worker.service"
        );
        assert_eq!(target_path("", "relative/file"), "relative/file");
        assert_eq!(ensure_trailing_newline(""), "");
        assert_eq!(ensure_trailing_newline("KEY=value"), "KEY=value\n");
        assert_eq!(ensure_trailing_newline("KEY=value\n"), "KEY=value\n");

        let (path, args) = slot_exec_start(
            "/opt/worker/bin/server-8080",
            "-p 8080 --metrics-port=19090",
            8080,
        );
        assert_eq!(path, "/opt/worker/bin/server-${PORT}");
        assert_eq!(args, "-p ${PORT} --metrics-port=19090");
    }

    fn binary_runtime_config() -> BinaryRuntimeConfig {
        BinaryRuntimeConfig {
            app_key: "worker-bin".to_owned(),
            app_id: 7,
            name: "Worker".to_owned(),
            service_name: "worker-bin".to_owned(),
            artifact_version: "v1.2.3".to_owned(),
            artifact_path: "/opt/worker/bin/server".to_owned(),
            exec_args: "--port 8080".to_owned(),
            working_dir: "/opt/worker".to_owned(),
            service_user: "deploy".to_owned(),
            unit_name: "easy-deploy-worker-bin.service".to_owned(),
            release_strategy: "blue_green".to_owned(),
            active_slot: "blue".to_owned(),
            base_port: 8080,
            standby_port: 18080,
            proxy_enabled: true,
            proxy_kind: "caddy".to_owned(),
            proxy_domain: "worker.example.com".to_owned(),
            proxy_config_path: "/etc/caddy/Caddyfile.d/worker-bin.caddy".to_owned(),
            env_content: "RUST_LOG=info".to_owned(),
        }
    }
}
