use sqlx::{QueryBuilder, Sqlite, SqlitePool};

#[derive(Clone)]
pub struct EventLogService {
    db: SqlitePool,
}

#[derive(Clone, Debug, Default)]
pub struct EventLogFilter {
    pub event_type: Option<String>,
    pub level: Option<String>,
    pub target_type: Option<String>,
    pub query: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct EventLogItem {
    pub id: i64,
    pub event_type: String,
    pub level: String,
    pub target_type: String,
    pub target_id: String,
    pub target_name: String,
    pub title: String,
    pub summary: String,
    pub detail: String,
    pub created_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct EventFilterOption {
    pub value: String,
}

pub struct EventLogInput<'a> {
    pub event_type: &'a str,
    pub level: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub target_name: &'a str,
    pub title: &'a str,
    pub summary: &'a str,
    pub detail: &'a str,
}

#[derive(Debug)]
pub enum EventLogError {
    InvalidInput(String),
    Internal(String),
}

impl EventLogError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Internal(message) => message,
        }
    }
}

impl std::fmt::Display for EventLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for EventLogError {}

impl From<sqlx::Error> for EventLogError {
    fn from(value: sqlx::Error) -> Self {
        Self::Internal(value.to_string())
    }
}

impl EventLogService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn record(&self, input: EventLogInput<'_>) -> Result<(), EventLogError> {
        insert_event_log(&self.db, input).await
    }

    pub async fn list_filtered(
        &self,
        filter: EventLogFilter,
    ) -> Result<Vec<EventLogItem>, EventLogError> {
        let filter = normalize_event_filter(filter)?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
            SELECT
                id,
                event_type,
                level,
                target_type,
                target_id,
                target_name,
                title,
                summary,
                detail,
                created_at
            FROM event_logs
            WHERE 1 = 1
            "#,
        );
        push_event_filter_clauses(&mut builder, &filter);
        builder.push(
            r#"
            ORDER BY id DESC
            LIMIT 100
            "#,
        );
        builder
            .build_query_as::<EventLogItem>()
            .fetch_all(&self.db)
            .await
            .map_err(EventLogError::from)
    }

    pub async fn event_type_options(&self) -> Result<Vec<EventFilterOption>, EventLogError> {
        sqlx::query_as::<_, EventFilterOption>(
            r#"
            SELECT DISTINCT event_type AS value
            FROM event_logs
            WHERE event_type != ''
            ORDER BY event_type
            LIMIT 200
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(EventLogError::from)
    }

    pub async fn target_type_options(&self) -> Result<Vec<EventFilterOption>, EventLogError> {
        sqlx::query_as::<_, EventFilterOption>(
            r#"
            SELECT DISTINCT target_type AS value
            FROM event_logs
            WHERE target_type != ''
            ORDER BY target_type
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(EventLogError::from)
    }
}

pub async fn insert_event_log(
    db: &SqlitePool,
    input: EventLogInput<'_>,
) -> Result<(), EventLogError> {
    let level = normalize_level(input.level)?;
    sqlx::query(
        r#"
        INSERT INTO event_logs(
            event_type,
            level,
            target_type,
            target_id,
            target_name,
            title,
            summary,
            detail
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        "#,
    )
    .bind(truncate(input.event_type.trim(), 80))
    .bind(level)
    .bind(truncate(input.target_type.trim(), 80))
    .bind(truncate(input.target_id.trim(), 80))
    .bind(truncate(input.target_name.trim(), 160))
    .bind(truncate(input.title.trim(), 160))
    .bind(truncate(input.summary.trim(), 1000))
    .bind(truncate(input.detail.trim(), 16_000))
    .execute(db)
    .await?;
    Ok(())
}

fn normalize_event_filter(mut filter: EventLogFilter) -> Result<EventLogFilter, EventLogError> {
    filter.event_type =
        normalize_optional_filter(filter.event_type).map(|value| value.chars().take(80).collect());
    filter.target_type =
        normalize_optional_filter(filter.target_type).map(|value| value.chars().take(80).collect());
    filter.query =
        normalize_optional_filter(filter.query).map(|value| value.chars().take(120).collect());
    filter.level = match normalize_optional_filter(filter.level).as_deref() {
        Some("debug") => Some("debug".to_owned()),
        Some("info") => Some("info".to_owned()),
        Some("warning") => Some("warning".to_owned()),
        Some("error") => Some("error".to_owned()),
        Some(_) => return Err(EventLogError::InvalidInput("事件级别不支持".to_owned())),
        None => None,
    };
    Ok(filter)
}

fn normalize_optional_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn normalize_level(level: &str) -> Result<&'static str, EventLogError> {
    match level.trim() {
        "debug" => Ok("debug"),
        "info" | "" => Ok("info"),
        "warning" => Ok("warning"),
        "error" => Ok("error"),
        _ => Err(EventLogError::InvalidInput("事件级别不支持".to_owned())),
    }
}

fn push_event_filter_clauses(builder: &mut QueryBuilder<'_, Sqlite>, filter: &EventLogFilter) {
    if let Some(event_type) = &filter.event_type {
        builder.push(" AND event_type = ");
        builder.push_bind(event_type.clone());
    }
    if let Some(level) = &filter.level {
        builder.push(" AND level = ");
        builder.push_bind(level.clone());
    }
    if let Some(target_type) = &filter.target_type {
        builder.push(" AND target_type = ");
        builder.push_bind(target_type.clone());
    }
    if let Some(query) = &filter.query {
        let like_query = format!("%{query}%");
        builder.push(" AND (event_type LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR level LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR target_type LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR target_id LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR target_name LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR title LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR summary LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR detail LIKE ");
        builder.push_bind(like_query);
        builder.push(")");
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        truncated.push_str("\n... truncated ...");
    }
    truncated
}
