use std::{fs, path::Path};

use anyhow::{Context, bail};
use sqlx::{Executor, Row, SqlitePool};

const BUSINESS_TABLES: &[&str] = &[
    "admin_sessions",
    "admin_audit_logs",
    "event_logs",
    "operation_task_node_results",
    "operation_task_logs",
    "deployment_runs",
    "operation_tasks",
    "binary_artifacts",
    "app_binary_configs",
    "app_health_checks",
    "app_config_snapshots",
    "app_runtime_states",
    "app_targets",
    "apps",
    "node_checks",
    "node_capabilities",
    "node_credentials",
];

#[derive(Clone, Debug)]
pub struct CleanDemoDataOptions {
    pub dry_run: bool,
    pub backup: bool,
    pub data_dir: std::path::PathBuf,
}

#[derive(Clone, Debug)]
pub struct CleanDemoDataReport {
    pub backup_path: Option<String>,
    pub table_counts: Vec<TableCount>,
    pub removed_paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TableCount {
    pub table: String,
    pub count: i64,
}

pub async fn clean_demo_data(
    db: &SqlitePool,
    database_url: &str,
    options: CleanDemoDataOptions,
) -> anyhow::Result<CleanDemoDataReport> {
    let table_counts = business_table_counts(db).await?;
    let mut removed_paths = existing_runtime_paths(&options.data_dir)?;
    let backup_path = if options.backup && !options.dry_run {
        Some(backup_sqlite_database(database_url)?)
    } else {
        None
    };

    if !options.dry_run {
        clear_business_tables(db).await?;
        reset_local_node(db).await?;
        remove_runtime_paths(&removed_paths)?;
    }

    if options.dry_run {
        removed_paths.sort();
    }

    Ok(CleanDemoDataReport {
        backup_path,
        table_counts,
        removed_paths,
    })
}

async fn business_table_counts(db: &SqlitePool) -> anyhow::Result<Vec<TableCount>> {
    let mut counts = Vec::with_capacity(BUSINESS_TABLES.len() + 1);
    for table in BUSINESS_TABLES {
        if table_exists(db, table).await? {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS count FROM {table}"))
                .fetch_one(db)
                .await?;
            counts.push(TableCount {
                table: (*table).to_owned(),
                count: row.get("count"),
            });
        }
    }
    if table_exists(db, "nodes").await? {
        let row = sqlx::query(
            "SELECT COUNT(*) AS count FROM nodes WHERE node_key <> 'local' OR credential_id IS NOT NULL",
        )
        .fetch_one(db)
        .await?;
        counts.push(TableCount {
            table: "nodes(non_local_or_bound_credentials)".to_owned(),
            count: row.get("count"),
        });
    }
    Ok(counts)
}

async fn clear_business_tables(db: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    tx.execute("PRAGMA foreign_keys = OFF").await?;
    for table in BUSINESS_TABLES {
        if table_exists_in_tx(&mut tx, table).await? {
            tx.execute(format!("DELETE FROM {table}").as_str()).await?;
        }
    }
    if table_exists_in_tx(&mut tx, "nodes").await? {
        tx.execute("DELETE FROM nodes WHERE node_key <> 'local'")
            .await?;
        tx.execute(
            r#"
            UPDATE nodes
            SET credential_id = NULL,
                status = 'unknown',
                docker_status = 'unknown',
                last_check_at = NULL,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE node_key = 'local'
            "#,
        )
        .await?;
    }
    tx.execute("PRAGMA foreign_keys = ON").await?;
    tx.commit().await?;
    Ok(())
}

async fn reset_local_node(db: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(db, "nodes").await? {
        return Ok(());
    }
    sqlx::query(
        r#"
        INSERT INTO nodes(
            node_key,
            name,
            node_type,
            address,
            ssh_port,
            ssh_user,
            work_dir,
            region,
            labels,
            status,
            docker_status,
            last_check_at
        )
        VALUES (
            'local',
            '本机节点',
            'local',
            '127.0.0.1',
            22,
            '',
            '.easy-deploy/apps',
            'local',
            'local,docker',
            'unknown',
            'unknown',
            NULL
        )
        ON CONFLICT(node_key) DO UPDATE SET
            name = excluded.name,
            node_type = excluded.node_type,
            address = excluded.address,
            ssh_port = excluded.ssh_port,
            ssh_user = excluded.ssh_user,
            work_dir = excluded.work_dir,
            region = excluded.region,
            labels = excluded.labels,
            status = excluded.status,
            docker_status = excluded.docker_status,
            last_check_at = excluded.last_check_at,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

async fn table_exists(db: &SqlitePool, table: &str) -> anyhow::Result<bool> {
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(table)
            .fetch_one(db)
            .await?;
    Ok(exists > 0)
}

async fn table_exists_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
) -> anyhow::Result<bool> {
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(table)
            .fetch_one(&mut **tx)
            .await?;
    Ok(exists > 0)
}

fn backup_sqlite_database(database_url: &str) -> anyhow::Result<String> {
    let path = sqlite_database_path(database_url)?;
    if !path.exists() {
        return Ok(String::new());
    }
    let timestamp = chrono_like_timestamp();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("db");
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("easy-deploy");
    let backup_path = path.with_file_name(format!("{stem}.backup-{timestamp}.{extension}"));
    fs::copy(path, &backup_path)
        .with_context(|| format!("备份数据库到 {}", backup_path.display()))?;
    Ok(backup_path.to_string_lossy().to_string())
}

fn sqlite_database_path(database_url: &str) -> anyhow::Result<&Path> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        bail!("clean-demo-data 目前只支持 sqlite:// 数据库");
    };
    if path == ":memory:" || path.is_empty() {
        bail!("内存数据库不支持备份清理");
    }
    Ok(Path::new(path))
}

fn existing_runtime_paths(data_dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    for relative in ["apps", "credentials"] {
        let path = data_dir.join(relative);
        if path.exists() {
            paths.push(path.to_string_lossy().to_string());
        }
    }
    Ok(paths)
}

fn remove_runtime_paths(paths: &[String]) -> anyhow::Result<()> {
    for path in paths {
        let path = Path::new(path);
        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("删除运行期测试目录 {}", path.display()))?;
        }
    }
    Ok(())
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    seconds.to_string()
}
