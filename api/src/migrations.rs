use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, anyhow, bail};
use clap::Subcommand;
use sqlx::{
    Sqlite, SqlitePool,
    migrate::{MigrateDatabase, Migration},
    sqlite::SqliteConnectOptions,
};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Subcommand)]
pub enum MigrationCommand {
    /// Show applied and pending SQL migrations.
    Status,

    /// Apply all pending SQL migrations.
    Up,

    /// Create a new numbered SQL migration file.
    Create {
        /// Migration name, for example: add_deployment_index.
        name: String,
    },

    /// Check migration changes against a base ref.
    Guard {
        /// Base ref to compare with. Defaults to origin/main, main, master, or HEAD.
        base_ref: Option<String>,
    },
}

pub async fn connect_database(
    database_url: &str,
    create_if_missing: bool,
) -> anyhow::Result<SqlitePool> {
    let options = database_url
        .parse::<SqliteConnectOptions>()
        .with_context(|| format!("parse database url {database_url}"))?
        .create_if_missing(create_if_missing)
        .foreign_keys(true);

    SqlitePool::connect_with(options)
        .await
        .with_context(|| format!("connect database {database_url}"))
}

pub async fn run_pending(db: &SqlitePool) -> anyhow::Result<()> {
    MIGRATOR.run(db).await.context("run database migrations")
}

pub async fn run_status(database_url: &str) -> anyhow::Result<()> {
    let database_exists = <Sqlite as MigrateDatabase>::database_exists(database_url)
        .await
        .with_context(|| format!("check database exists {database_url}"))?;

    let applied = if database_exists {
        let db = connect_database(database_url, false).await?;
        load_applied_migrations(&db).await?
    } else {
        BTreeMap::new()
    };

    print_status(database_url, &applied, &mut io::stdout())
}

pub async fn run_up(db: &SqlitePool) -> anyhow::Result<()> {
    run_pending(db).await?;
    println!("migrations up done");
    Ok(())
}

pub fn run_create(name: &str) -> anyhow::Result<()> {
    let migration_name = normalize_migration_name(name)?;
    let migrations_dir = source_migrations_dir()?;
    fs::create_dir_all(&migrations_dir)
        .with_context(|| format!("create migrations dir {}", migrations_dir.display()))?;

    let existing_names = read_migration_file_names(&migrations_dir)?;
    let file_name =
        next_versioned_file_name(existing_names.iter().map(String::as_str), &migration_name)?;
    let path = migrations_dir.join(file_name);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create migration {}", path.display()))?;
    file.write_all(b"-- Add migration script here.\n")
        .with_context(|| format!("write migration {}", path.display()))?;

    println!("created {}", path.display());
    Ok(())
}

pub fn run_guard(base_ref: Option<&str>) -> anyhow::Result<()> {
    let repo_root = git_repo_root()?;
    let migrations_dir = source_migrations_dir()?;
    let migrations_dir = migrations_dir
        .canonicalize()
        .with_context(|| format!("canonicalize migrations dir {}", migrations_dir.display()))?;
    let rel_migrations_dir = to_repo_relative_slash_path(&repo_root, &migrations_dir)?;
    let base_ref = match base_ref {
        Some(base_ref) if !base_ref.trim().is_empty() => base_ref.trim().to_string(),
        _ => default_base_ref(),
    };

    let mut changes = parse_name_status_lines(&git_output_lines(&[
        "diff",
        "--name-status",
        &format!("{base_ref}...HEAD"),
        "--",
        &rel_migrations_dir,
    ])?)?;

    let added_since_base = changes
        .iter()
        .filter(|change| change.status == "A")
        .map(|change| normalize_slash_path(&change.path))
        .collect::<BTreeSet<_>>();

    let mut worktree_changes = parse_name_status_lines(&git_output_lines(&[
        "diff",
        "--name-status",
        "HEAD",
        "--",
        &rel_migrations_dir,
    ])?)?;
    for change in &mut worktree_changes {
        if change.status == "M" && added_since_base.contains(&normalize_slash_path(&change.path)) {
            change.status = "A".to_string();
        }
    }
    changes.extend(worktree_changes);

    let untracked = git_output_lines(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "--",
        &rel_migrations_dir,
    ])?;
    changes.extend(parse_name_status_lines(&append_untracked_as_adds(
        untracked,
    ))?);

    validate_changes(&changes, &rel_migrations_dir)?;
    println!("migration guard passed: {rel_migrations_dir} against {base_ref}");
    Ok(())
}

async fn load_applied_migrations(db: &SqlitePool) -> anyhow::Result<BTreeMap<i64, AppliedRow>> {
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(1) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(db)
    .await
    .context("check _sqlx_migrations table")?;

    if table_count == 0 {
        return Ok(BTreeMap::new());
    }

    let rows = sqlx::query_as::<_, (i64, String, bool, Vec<u8>)>(
        "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(db)
    .await
    .context("load applied migrations")?;

    Ok(rows
        .into_iter()
        .map(|(version, description, success, checksum)| {
            (
                version,
                AppliedRow {
                    description,
                    success,
                    checksum,
                },
            )
        })
        .collect())
}

fn print_status(
    database_url: &str,
    applied: &BTreeMap<i64, AppliedRow>,
    writer: &mut impl Write,
) -> anyhow::Result<()> {
    let known = MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .map(|migration| migration.version)
        .collect::<BTreeSet<_>>();

    let mut rows = Vec::new();
    for migration in MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
    {
        rows.push(status_row_for_migration(
            migration,
            applied.get(&migration.version),
        ));
    }

    for (version, applied_row) in applied {
        if !known.contains(version) {
            rows.push(StatusRow {
                state: "missing",
                version: *version,
                description: applied_row.description.clone(),
            });
        }
    }

    let applied_count = rows.iter().filter(|row| row.state == "applied").count();
    let pending_count = rows.iter().filter(|row| row.state == "pending").count();
    let changed_count = rows.iter().filter(|row| row.state == "changed").count();
    let dirty_count = rows.iter().filter(|row| row.state == "dirty").count();
    let missing_count = rows.iter().filter(|row| row.state == "missing").count();

    writeln!(writer, "migration directory: api/migrations")?;
    writeln!(writer, "database: {database_url}")?;
    writeln!(
        writer,
        "summary: applied={applied_count} pending={pending_count} changed={changed_count} dirty={dirty_count} missing={missing_count}"
    )?;
    writeln!(writer, "{:<10} {:<8} DESCRIPTION", "STATE", "VERSION")?;
    for row in rows {
        writeln!(
            writer,
            "{:<10} {:<8} {}",
            row.state, row.version, row.description
        )?;
    }

    Ok(())
}

fn status_row_for_migration(migration: &Migration, applied: Option<&AppliedRow>) -> StatusRow {
    let description = migration.description.to_string();
    let state = match applied {
        None => "pending",
        Some(applied) if !applied.success => "dirty",
        Some(applied) if applied.checksum.as_slice() != migration.checksum.as_ref() => "changed",
        Some(_) => "applied",
    };

    StatusRow {
        state,
        version: migration.version,
        description,
    }
}

fn read_migration_file_names(migrations_dir: &Path) -> anyhow::Result<Vec<String>> {
    fs::read_dir(migrations_dir)
        .with_context(|| format!("read migrations dir {}", migrations_dir.display()))?
        .map(|entry| {
            let entry = entry
                .with_context(|| format!("read migrations dir {}", migrations_dir.display()))?;
            Ok(entry.file_name().to_string_lossy().into_owned())
        })
        .collect()
}

fn source_migrations_dir() -> anyhow::Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
}

fn normalize_migration_name(name: &str) -> anyhow::Result<String> {
    let mut normalized = String::new();
    let mut last_was_underscore = false;

    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            normalized.push('_');
            last_was_underscore = true;
        }
    }

    let normalized = normalized.trim_matches('_').to_string();
    if normalized.is_empty() {
        bail!("migration name must contain ASCII letters or digits");
    }
    if !is_valid_migration_description(&normalized) {
        bail!("migration name must be snake_case ASCII");
    }
    Ok(normalized)
}

fn next_versioned_file_name<'a>(
    existing_names: impl Iterator<Item = &'a str>,
    migration_name: &str,
) -> anyhow::Result<String> {
    let mut max_version = 0_i64;
    let mut width = 4_usize;

    for name in existing_names {
        let Some((version, version_width)) = parse_migration_version(name)? else {
            continue;
        };
        max_version = max_version.max(version);
        width = width.max(version_width);
    }

    let next_version = max_version + 1;
    Ok(format!(
        "{next_version:0width$}_{migration_name}.sql",
        width = width
    ))
}

fn parse_migration_version(file_name: &str) -> anyhow::Result<Option<(i64, usize)>> {
    let Some((prefix, rest)) = file_name.split_once('_') else {
        return Ok(None);
    };
    if !rest.ends_with(".sql") {
        return Ok(None);
    }
    if prefix.is_empty() || !prefix.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("invalid migration filename {file_name:?}: version prefix must be digits");
    }
    let version = prefix
        .parse::<i64>()
        .with_context(|| format!("parse migration version from {file_name:?}"))?;
    Ok(Some((version, prefix.len())))
}

fn append_untracked_as_adds(untracked_paths: Vec<String>) -> Vec<String> {
    untracked_paths
        .into_iter()
        .filter(|path| !path.trim().is_empty())
        .map(|path| format!("A\t{}", path.trim()))
        .collect()
}

fn parse_name_status_lines(lines: &[String]) -> anyhow::Result<Vec<MigrationChange>> {
    let mut changes = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 2 {
            bail!("invalid git name-status line: {line:?}");
        }

        let status = fields[0].to_string();
        if status.starts_with('R') || status.starts_with('C') {
            if fields.len() != 3 {
                bail!("invalid rename/copy git name-status line: {line:?}");
            }
            changes.push(MigrationChange {
                status,
                old_path: fields[1].to_string(),
                path: fields[2].to_string(),
            });
        } else {
            changes.push(MigrationChange {
                status,
                old_path: String::new(),
                path: fields[fields.len() - 1].to_string(),
            });
        }
    }
    Ok(changes)
}

fn validate_changes(changes: &[MigrationChange], migrations_dir: &str) -> anyhow::Result<()> {
    let violations = changes
        .iter()
        .flat_map(|change| validate_change(change, migrations_dir))
        .collect::<Vec<_>>();

    if violations.is_empty() {
        return Ok(());
    }

    Err(anyhow!(
        "migration guard failed:\n- {}",
        violations.join("\n- ")
    ))
}

fn validate_change(change: &MigrationChange, migrations_dir: &str) -> Vec<String> {
    let mut paths = vec![change.path.as_str()];
    if !change.old_path.is_empty() {
        paths.push(change.old_path.as_str());
    }
    if !touches_migrations_dir(&paths, migrations_dir) {
        return Vec::new();
    }

    let base = slash_file_name(&change.path);
    if base == ".keep" {
        return Vec::new();
    }
    if !base.ends_with(".sql") {
        return vec![format!(
            "migration directory only allows versioned SQL, got {:?}",
            change.path
        )];
    }

    match change.status.as_str() {
        "A" => {
            if is_valid_migration_file_name(base) {
                Vec::new()
            } else {
                vec![format!(
                    "new migration file {:?} must match NNNN_snake_case.sql",
                    change.path
                )]
            }
        }
        status if status.starts_with('R') => vec![format!(
            "historical migration SQL cannot be renamed: {:?} -> {:?}",
            change.old_path, change.path
        )],
        "M" => vec![format!(
            "historical migration SQL cannot be modified: {:?}",
            change.path
        )],
        "D" => vec![format!(
            "historical migration SQL cannot be deleted: {:?}",
            change.path
        )],
        status => vec![format!(
            "migration SQL only allows new files, got {status} on {:?}",
            change.path
        )],
    }
}

fn touches_migrations_dir(paths: &[&str], migrations_dir: &str) -> bool {
    let migrations_dir = normalize_slash_path(migrations_dir);
    paths.iter().any(|path| {
        let path = normalize_slash_path(path);
        path == migrations_dir || path.starts_with(&format!("{migrations_dir}/"))
    })
}

fn is_valid_migration_file_name(file_name: &str) -> bool {
    let Some((prefix, description)) = file_name.split_once('_') else {
        return false;
    };
    if prefix.len() < 4 || !prefix.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let Some(description) = description.strip_suffix(".sql") else {
        return false;
    };
    is_valid_migration_description(description)
}

fn is_valid_migration_description(description: &str) -> bool {
    let mut chars = description.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    !description.contains("__")
        && !description.ends_with('_')
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn slash_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn normalize_slash_path(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

fn git_repo_root() -> anyhow::Result<PathBuf> {
    let output = git_output(&["rev-parse", "--show-toplevel"])?;
    fs::canonicalize(output.trim()).context("canonicalize git repo root")
}

fn to_repo_relative_slash_path(repo_root: &Path, path: &Path) -> anyhow::Result<String> {
    let rel = path.strip_prefix(repo_root).with_context(|| {
        format!(
            "resolve {} relative to repo root {}",
            path.display(),
            repo_root.display()
        )
    })?;
    Ok(normalize_slash_path(&rel.to_string_lossy()))
}

fn default_base_ref() -> String {
    if let Ok(remote_head) = git_output(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ]) && !remote_head.trim().is_empty()
    {
        return remote_head.trim().to_string();
    }
    for candidate in ["origin/main", "main", "master", "HEAD"] {
        if git_ref_exists(candidate) {
            return candidate.to_string();
        }
    }
    "HEAD".to_string()
}

fn git_ref_exists(reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_output_lines(args: &[&str]) -> anyhow::Result<Vec<String>> {
    let output = git_output(args)?;
    if output.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(output.lines().map(str::to_string).collect())
}

fn git_output(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Debug)]
struct AppliedRow {
    description: String,
    success: bool,
    checksum: Vec<u8>,
}

#[derive(Debug)]
struct StatusRow {
    state: &'static str,
    version: i64,
    description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MigrationChange {
    status: String,
    path: String,
    old_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn load_applied_migrations_handles_missing_and_existing_table() {
        let db = SqlitePool::connect_with(
            "sqlite::memory:"
                .parse::<SqliteConnectOptions>()
                .expect("valid in-memory sqlite url"),
        )
        .await
        .expect("connect sqlite");

        assert!(
            load_applied_migrations(&db)
                .await
                .expect("load without table")
                .is_empty()
        );

        sqlx::query(
            r#"
            CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("create migrations table");
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(42_i64)
        .bind("add_release_queue")
        .bind(true)
        .bind(vec![1_u8, 2, 3])
        .execute(&db)
        .await
        .expect("insert migration row");

        let rows = load_applied_migrations(&db)
            .await
            .expect("load applied migrations");
        let row = rows.get(&42).expect("applied row");

        assert_eq!(row.description, "add_release_queue");
        assert!(row.success);
        assert_eq!(row.checksum, vec![1_u8, 2, 3]);
    }

    #[test]
    fn print_status_includes_summary_and_missing_rows() {
        let migration = MIGRATOR
            .iter()
            .find(|migration| migration.migration_type.is_up_migration())
            .expect("at least one migration");
        let mut applied = BTreeMap::new();
        applied.insert(
            migration.version,
            AppliedRow {
                description: migration.description.to_string(),
                success: true,
                checksum: migration.checksum.as_ref().to_vec(),
            },
        );
        applied.insert(
            999_999,
            AppliedRow {
                description: "manual_hotfix".to_owned(),
                success: true,
                checksum: vec![9],
            },
        );
        let mut output = Vec::new();

        print_status("sqlite::memory:", &applied, &mut output).expect("print status");
        let text = String::from_utf8(output).expect("status utf8");

        assert!(text.contains("migration directory: api/migrations"));
        assert!(text.contains("database: sqlite::memory:"));
        assert!(text.contains("summary:"));
        assert!(text.contains("applied=1"));
        assert!(text.contains("missing=1"));
        assert!(text.contains("999999"));
        assert!(text.contains("manual_hotfix"));
    }

    #[test]
    fn status_row_detects_pending_dirty_changed_and_applied() {
        let migration = MIGRATOR
            .iter()
            .find(|migration| migration.migration_type.is_up_migration())
            .expect("at least one migration");

        assert_eq!(status_row_for_migration(migration, None).state, "pending");
        assert_eq!(
            status_row_for_migration(
                migration,
                Some(&AppliedRow {
                    description: migration.description.to_string(),
                    success: false,
                    checksum: migration.checksum.as_ref().to_vec(),
                }),
            )
            .state,
            "dirty"
        );
        assert_eq!(
            status_row_for_migration(
                migration,
                Some(&AppliedRow {
                    description: migration.description.to_string(),
                    success: true,
                    checksum: vec![0],
                }),
            )
            .state,
            "changed"
        );
        assert_eq!(
            status_row_for_migration(
                migration,
                Some(&AppliedRow {
                    description: migration.description.to_string(),
                    success: true,
                    checksum: migration.checksum.as_ref().to_vec(),
                }),
            )
            .state,
            "applied"
        );
    }

    #[test]
    fn normalizes_migration_name() {
        assert_eq!(
            normalize_migration_name(" Add Deployment Index ").unwrap(),
            "add_deployment_index"
        );
        assert_eq!(
            normalize_migration_name("add-deployment.index").unwrap(),
            "add_deployment_index"
        );
    }

    #[test]
    fn rejects_empty_migration_name() {
        assert!(normalize_migration_name(" --- ").is_err());
    }

    #[test]
    fn validates_migration_description_boundaries() {
        assert!(is_valid_migration_description("add_release_queue"));
        assert!(is_valid_migration_description("v2_index"));
        assert!(!is_valid_migration_description(""));
        assert!(!is_valid_migration_description("_leading"));
        assert!(!is_valid_migration_description("trailing_"));
        assert!(!is_valid_migration_description("double__underscore"));
        assert!(!is_valid_migration_description("UpperCase"));
        assert!(!is_valid_migration_description("contains-dash"));
    }

    #[test]
    fn creates_next_numbered_file_name() {
        let names = ["0001_init.sql", "0028_api_tokens.sql", "README.md"];
        assert_eq!(
            next_versioned_file_name(names.iter().copied(), "add_audit_index").unwrap(),
            "0029_add_audit_index.sql"
        );
    }

    #[test]
    fn next_versioned_file_name_preserves_wider_version_width() {
        let names = ["000001_init.sql", "000010_add_users.sql", "notes.txt"];

        assert_eq!(
            next_versioned_file_name(names.iter().copied(), "add_audit_index").unwrap(),
            "000011_add_audit_index.sql"
        );
    }

    #[test]
    fn parses_migration_versions_from_numbered_sql_files() {
        assert_eq!(
            parse_migration_version("0042_release_queue_scheduled_status.sql").unwrap(),
            Some((42, 4))
        );
        assert_eq!(parse_migration_version("README.md").unwrap(), None);
        assert_eq!(parse_migration_version("0042_add_table.txt").unwrap(), None);
        assert_eq!(parse_migration_version("0042").unwrap(), None);
        assert!(parse_migration_version("abcd_bad.sql").is_err());
    }

    #[test]
    fn read_migration_file_names_lists_directory_entries() {
        let dir = tempdir().expect("create temp dir");
        fs::write(dir.path().join("0001_init.sql"), "-- init\n").expect("write migration");
        fs::write(dir.path().join(".keep"), "").expect("write keep file");

        let mut names = read_migration_file_names(dir.path()).expect("read migration file names");
        names.sort();

        assert_eq!(names, vec![".keep".to_owned(), "0001_init.sql".to_owned()]);
        assert!(read_migration_file_names(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn validates_migration_file_name_shape() {
        assert!(is_valid_migration_file_name("0001_init.sql"));
        assert!(is_valid_migration_file_name("202607040001_add_app.sql"));
        assert!(!is_valid_migration_file_name("001_add_app.sql"));
        assert!(!is_valid_migration_file_name("0001_AddApp.sql"));
        assert!(!is_valid_migration_file_name("0001_add__app.sql"));
        assert!(!is_valid_migration_file_name("0001_add_app.txt"));
    }

    #[test]
    fn parses_git_name_status_lines() {
        let changes = parse_name_status_lines(&[
            "A\tapi/migrations/0029_add_table.sql".to_string(),
            "R100\tapi/migrations/0001_init.sql\tapi/migrations/0001_bootstrap.sql".to_string(),
        ])
        .unwrap();

        assert_eq!(
            changes,
            vec![
                MigrationChange {
                    status: "A".to_string(),
                    path: "api/migrations/0029_add_table.sql".to_string(),
                    old_path: String::new(),
                },
                MigrationChange {
                    status: "R100".to_string(),
                    old_path: "api/migrations/0001_init.sql".to_string(),
                    path: "api/migrations/0001_bootstrap.sql".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_name_status_rejects_malformed_lines() {
        assert!(parse_name_status_lines(&["M".to_string()]).is_err());
        assert!(parse_name_status_lines(&["R100\tapi/migrations/0001_a.sql".to_string()]).is_err());
    }

    #[test]
    fn guard_allows_new_numbered_migration() {
        let changes = vec![MigrationChange {
            status: "A".to_string(),
            path: "api/migrations/0029_add_table.sql".to_string(),
            old_path: String::new(),
        }];

        validate_changes(&changes, "api/migrations").unwrap();
    }

    #[test]
    fn guard_rejects_historical_modification() {
        let changes = vec![MigrationChange {
            status: "M".to_string(),
            path: "api/migrations/0001_init.sql".to_string(),
            old_path: String::new(),
        }];

        let err = validate_changes(&changes, "api/migrations").unwrap_err();
        assert!(err.to_string().contains("cannot be modified"));
    }

    #[test]
    fn guard_rejects_bad_new_file_name() {
        let changes = vec![MigrationChange {
            status: "A".to_string(),
            path: "api/migrations/add_table.sql".to_string(),
            old_path: String::new(),
        }];

        let err = validate_changes(&changes, "api/migrations").unwrap_err();
        assert!(err.to_string().contains("NNNN_snake_case.sql"));
    }

    #[test]
    fn guard_rejects_delete_rename_and_non_sql_inside_migrations() {
        let changes = vec![
            MigrationChange {
                status: "D".to_string(),
                path: "api/migrations/0001_init.sql".to_string(),
                old_path: String::new(),
            },
            MigrationChange {
                status: "R100".to_string(),
                path: "api/migrations/0001_bootstrap.sql".to_string(),
                old_path: "api/migrations/0001_init.sql".to_string(),
            },
            MigrationChange {
                status: "A".to_string(),
                path: "api/migrations/readme.md".to_string(),
                old_path: String::new(),
            },
        ];

        let err = validate_changes(&changes, "api/migrations").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("cannot be deleted"));
        assert!(message.contains("cannot be renamed"));
        assert!(message.contains("only allows versioned SQL"));
    }

    #[test]
    fn guard_rejects_copy_and_unknown_status_inside_migrations() {
        let changes = vec![
            MigrationChange {
                status: "C100".to_string(),
                path: "api/migrations/0002_copy.sql".to_string(),
                old_path: "api/migrations/0001_init.sql".to_string(),
            },
            MigrationChange {
                status: "T".to_string(),
                path: "api/migrations/0001_init.sql".to_string(),
                old_path: String::new(),
            },
        ];

        let err = validate_changes(&changes, "api/migrations").unwrap_err();
        let message = err.to_string();

        assert!(message.contains("got C100"));
        assert!(message.contains("got T"));
    }

    #[test]
    fn guard_ignores_changes_outside_migrations_and_keep_file() {
        let changes = vec![
            MigrationChange {
                status: "M".to_string(),
                path: "api/src/main.rs".to_string(),
                old_path: String::new(),
            },
            MigrationChange {
                status: "M".to_string(),
                path: "api/migrations/.keep".to_string(),
                old_path: String::new(),
            },
        ];

        validate_changes(&changes, "api/migrations").unwrap();
    }

    #[test]
    fn normalizes_paths_for_migration_dir_matching() {
        assert_eq!(
            slash_file_name(r"api\migrations\0001_init.sql"),
            "0001_init.sql"
        );
        assert_eq!(normalize_slash_path(r"\api\migrations\"), "api/migrations");
        assert!(touches_migrations_dir(
            &[r"api\migrations\0001_init.sql"],
            "api/migrations"
        ));
        assert!(touches_migrations_dir(
            &["/api/migrations"],
            "api/migrations"
        ));
        assert!(!touches_migrations_dir(
            &["api/src/migrations.rs"],
            "api/migrations"
        ));
    }

    #[test]
    fn appends_untracked_paths_as_adds() {
        assert_eq!(
            append_untracked_as_adds(vec!["api/migrations/0029_add_table.sql".to_string()]),
            vec!["A\tapi/migrations/0029_add_table.sql".to_string()]
        );
        assert_eq!(
            append_untracked_as_adds(vec![
                " ".to_string(),
                " api/migrations/0030_add_node.sql ".to_string()
            ]),
            vec!["A\tapi/migrations/0030_add_node.sql".to_string()]
        );
    }

    #[test]
    fn repo_relative_path_uses_slashes_and_rejects_external_path() {
        let root = tempdir().expect("create repo root");
        let migration_path = root.path().join("api").join("migrations");
        let other = tempdir().expect("create other dir");

        assert_eq!(
            to_repo_relative_slash_path(root.path(), &migration_path).expect("relative path"),
            "api/migrations"
        );
        assert!(to_repo_relative_slash_path(root.path(), other.path()).is_err());
    }
}
