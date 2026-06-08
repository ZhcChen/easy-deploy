use std::{net::SocketAddr, path::PathBuf};

use clap::Args;

pub const DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP: usize = 4;
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;

#[derive(Clone, Debug, Args)]
#[command(name = "easy-deploy-api", about = "Easy Deploy API 服务")]
pub struct Settings {
    #[arg(long, env = "EASY_DEPLOY_BIND", default_value = "127.0.0.1:9066")]
    pub bind: SocketAddr,

    #[arg(
        long,
        env = "EASY_DEPLOY_DATABASE_URL",
        default_value = "sqlite://easy-deploy.db"
    )]
    pub database_url: String,

    #[arg(long, env = "EASY_DEPLOY_DATA_DIR", default_value = ".easy-deploy")]
    pub data_dir: PathBuf,

    #[arg(long, env = "EASY_DEPLOY_COOKIE_SECURE", default_value_t = false)]
    pub cookie_secure: bool,

    #[arg(
        long,
        env = "EASY_DEPLOY_UPLOADED_BINARY_RELEASES_TO_KEEP",
        default_value_t = DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP
    )]
    pub uploaded_binary_releases_to_keep: usize,

    #[arg(
        long,
        env = "EASY_DEPLOY_COMMAND_TIMEOUT_SECS",
        default_value_t = DEFAULT_COMMAND_TIMEOUT_SECS
    )]
    pub command_timeout_secs: u64,
}
