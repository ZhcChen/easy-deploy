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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tempfile::{TempDir, tempdir};

    use super::*;

    fn temp_settings() -> (Settings, TempDir) {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("easy-deploy-test.db");
        let database_url = format!("sqlite://{}", database_path.to_string_lossy());
        let settings = Settings {
            bind: "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("parse bind addr"),
            database_url,
            data_dir: temp_dir.path().join("data"),
            cookie_secure: false,
            uploaded_binary_releases_to_keep: 4,
            command_timeout_secs: 120,
        };
        (settings, temp_dir)
    }

    #[test]
    fn cli_parses_clean_demo_data_and_migrate_commands() {
        let temp_dir = tempdir().expect("create temp dir");
        let cli = Cli::try_parse_from([
            "easy-deploy-api",
            "--bind",
            "127.0.0.1:0",
            "--database-url",
            "sqlite://easy-deploy-test.db",
            "--data-dir",
            temp_dir.path().to_string_lossy().as_ref(),
            "--cookie-secure",
            "--uploaded-binary-releases-to-keep",
            "8",
            "--command-timeout-secs",
            "15",
            "clean-demo-data",
            "--dry-run",
            "--no-backup",
        ])
        .expect("parse clean-demo-data cli");

        assert_eq!(cli.settings.bind.port(), 0);
        assert!(cli.settings.cookie_secure);
        assert_eq!(cli.settings.uploaded_binary_releases_to_keep, 8);
        assert_eq!(cli.settings.command_timeout_secs, 15);
        assert!(matches!(
            cli.command,
            Some(Command::CleanDemoData {
                dry_run: true,
                no_backup: true
            })
        ));

        let cli = Cli::try_parse_from(["easy-deploy-api", "migrate", "guard", "HEAD"])
            .expect("parse migrate guard cli");
        assert!(matches!(
            cli.command,
            Some(Command::Migrate {
                command: MigrationCommand::Guard { base_ref: Some(_) }
            })
        ));
    }

    #[tokio::test]
    async fn migration_commands_run_against_temp_database() {
        let (settings, _temp_dir) = temp_settings();

        run_migration_command(MigrationCommand::Status, &settings)
            .await
            .expect("status handles missing database");
        run_migration_command(MigrationCommand::Up, &settings)
            .await
            .expect("migrate up");
        run_migration_command(MigrationCommand::Status, &settings)
            .await
            .expect("status handles migrated database");
    }

    #[tokio::test]
    async fn clean_demo_data_command_runs_with_temp_database() {
        let (settings, _temp_dir) = temp_settings();
        let db = migrations::connect_database(&settings.database_url, true)
            .await
            .expect("connect temp database");
        migrations::run_pending(&db)
            .await
            .expect("run migrations before clean");

        run_command(
            Command::CleanDemoData {
                dry_run: true,
                no_backup: true,
            },
            db,
            settings,
        )
        .await
        .expect("run clean-demo-data dry run");
    }
}
