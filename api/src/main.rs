use anyhow::Context;
use std::sync::Arc;

use api::{
    AppState, AppStateServices, Settings,
    apps::AppService,
    auth::{AuthService, MemorySessionStore},
    build_router,
    deploy::{ComposeExecutor, SystemdExecutor, TokioCommandRunner, ssh_known_hosts_file},
    events::EventLogService,
    maintenance::{CleanDemoDataOptions, clean_demo_data},
    migrations::{self, MigrationCommand},
    node_credentials::NodeCredentialService,
    nodes::NodeService,
    platform::PlatformConfigService,
    runtimefs::RuntimeFs,
    tasks::TaskService,
};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "easy-deploy-api", about = "Easy Deploy API 服务")]
struct Cli {
    #[command(flatten)]
    settings: Settings,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 清理演示/测试业务数据，保留账号、RBAC、系统设置和默认本机节点。
    CleanDemoData {
        /// 只打印将清理的数据，不执行删除。
        #[arg(long)]
        dry_run: bool,

        /// 不备份 SQLite 数据库。
        #[arg(long)]
        no_backup: bool,
    },

    /// Manage SQL migrations.
    Migrate {
        #[command(subcommand)]
        command: MigrationCommand,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let settings = cli.settings;
    if let Some(Command::Migrate { command }) = cli.command {
        return run_migration_command(command, &settings).await;
    }

    std::fs::create_dir_all(&settings.data_dir).with_context(|| {
        format!(
            "create data directory {}",
            settings.data_dir.to_string_lossy()
        )
    })?;

    let db = migrations::connect_database(&settings.database_url, true).await?;
    migrations::run_pending(&db).await?;

    if let Some(command) = cli.command {
        return run_command(command, db, settings).await;
    }

    serve(db, settings).await
}

async fn run_migration_command(
    command: MigrationCommand,
    settings: &Settings,
) -> anyhow::Result<()> {
    match command {
        MigrationCommand::Status => migrations::run_status(&settings.database_url).await,
        MigrationCommand::Up => {
            std::fs::create_dir_all(&settings.data_dir).with_context(|| {
                format!(
                    "create data directory {}",
                    settings.data_dir.to_string_lossy()
                )
            })?;
            let db = migrations::connect_database(&settings.database_url, true).await?;
            migrations::run_up(&db).await
        }
        MigrationCommand::Create { name } => migrations::run_create(&name),
        MigrationCommand::Guard { base_ref } => migrations::run_guard(base_ref.as_deref()),
    }
}

async fn run_command(
    command: Command,
    db: sqlx::SqlitePool,
    settings: Settings,
) -> anyhow::Result<()> {
    match command {
        Command::CleanDemoData { dry_run, no_backup } => {
            let report = clean_demo_data(
                &db,
                &settings.database_url,
                CleanDemoDataOptions {
                    dry_run,
                    backup: !no_backup,
                    data_dir: settings.data_dir,
                },
            )
            .await?;
            println!(
                "clean-demo-data {}",
                if dry_run { "dry-run" } else { "done" }
            );
            if let Some(backup_path) = report.backup_path.filter(|path| !path.is_empty()) {
                println!("backup: {backup_path}");
            }
            println!("tables:");
            for item in report.table_counts {
                println!("  {}: {}", item.table, item.count);
            }
            if !report.removed_paths.is_empty() {
                println!("runtime paths:");
                for path in report.removed_paths {
                    println!("  {path}");
                }
            }
        }
        Command::Migrate { .. } => unreachable!("migrate command is handled before database setup"),
    }
    Ok(())
}

async fn serve(db: sqlx::SqlitePool, settings: Settings) -> anyhow::Result<()> {
    let command_runner = Arc::new(TokioCommandRunner::new(settings.command_timeout_secs));
    let auth = AuthService::new(db.clone(), Arc::new(MemorySessionStore::new()));
    auth.sync_permission_registry()
        .await
        .context("sync permission registry")?;
    let nodes =
        NodeService::new_with_data_dir(db.clone(), command_runner.clone(), &settings.data_dir);
    let node_credentials = NodeCredentialService::new(db.clone(), settings.data_dir.clone());
    let tasks = TaskService::new(db.clone());
    let platform = PlatformConfigService::new(db.clone());
    let events = EventLogService::new(db.clone());
    let apps = AppService::new(
        db.clone(),
        RuntimeFs::new(settings.data_dir.clone()),
        ComposeExecutor::new(command_runner.clone()),
        SystemdExecutor::new(command_runner.clone())
            .with_ssh_known_hosts_file(ssh_known_hosts_file(&settings.data_dir)),
        tasks.clone(),
        platform.clone(),
    );
    let listener = TcpListener::bind(settings.bind)
        .await
        .with_context(|| format!("bind {}", settings.bind))?;
    let local_addr = listener.local_addr().context("read listener address")?;
    let app = build_router(AppState::new(
        settings,
        db,
        AppStateServices {
            auth,
            nodes,
            node_credentials,
            apps,
            tasks,
            platform,
            events,
        },
    ));

    info!("easy-deploy api listening on http://{local_addr}");
    axum::serve(listener, app)
        .await
        .context("serve easy-deploy api")?;

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("api=debug,tower_http=info,info")),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}
