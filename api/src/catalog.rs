#[derive(Clone, Copy, Debug)]
pub struct ComposeTemplate {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub image: &'static str,
    pub default_port: u16,
    pub env_hint: &'static str,
}

#[derive(Clone, Debug)]
pub struct RenderedTemplate {
    pub compose_content: String,
    pub env_content: String,
}

#[derive(Clone, Debug)]
pub struct RenderTemplateInput<'a> {
    pub template_key: &'a str,
    pub app_key: &'a str,
    pub port: u16,
}

#[derive(Debug)]
pub enum CatalogError {
    TemplateNotFound(String),
    InvalidInput(String),
}

impl CatalogError {
    pub fn message(&self) -> &str {
        match self {
            Self::TemplateNotFound(message) | Self::InvalidInput(message) => message,
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CatalogError {}

pub fn compose_templates() -> &'static [ComposeTemplate] {
    &[
        ComposeTemplate {
            key: "nginx-static",
            name: "Nginx 静态站点",
            description: "适合快速托管静态文件或验证反向代理入口。",
            image: "nginx:1.27-alpine",
            default_port: 8080,
            env_hint: "PUBLIC_PORT=8080",
        },
        ComposeTemplate {
            key: "redis-single",
            name: "Redis 单节点",
            description: "带持久化目录的单节点 Redis，适合开发和小型内部服务。",
            image: "redis:7-alpine",
            default_port: 6379,
            env_hint: "REDIS_PORT=6379",
        },
        ComposeTemplate {
            key: "postgres-single",
            name: "PostgreSQL 单节点",
            description: "内置初始化账号和数据卷，适合小型服务快速启动数据库。",
            image: "postgres:16-alpine",
            default_port: 5432,
            env_hint: "POSTGRES_PASSWORD=change-me",
        },
        ComposeTemplate {
            key: "caddy-gateway",
            name: "Caddy 网关",
            description: "轻量反向代理网关入口，后续可配合低停机切流。",
            image: "caddy:2-alpine",
            default_port: 8088,
            env_hint: "PUBLIC_PORT=8088",
        },
    ]
}

pub fn template_by_key(key: &str) -> Option<&'static ComposeTemplate> {
    compose_templates()
        .iter()
        .find(|template| template.key == key)
}

pub fn render_compose_template(
    input: RenderTemplateInput<'_>,
) -> Result<RenderedTemplate, CatalogError> {
    if input.app_key.trim().is_empty() {
        return Err(CatalogError::InvalidInput("请输入应用标识".to_owned()));
    }
    if input.port == 0 {
        return Err(CatalogError::InvalidInput(
            "端口必须在 1-65535 之间".to_owned(),
        ));
    }
    let Some(template) = template_by_key(input.template_key) else {
        return Err(CatalogError::TemplateNotFound("模板不存在".to_owned()));
    };
    let service_name = service_name(input.app_key);
    let rendered = match template.key {
        "redis-single" => render_redis(&service_name, input.port),
        "postgres-single" => render_postgres(&service_name, input.port),
        "caddy-gateway" => render_caddy(&service_name, input.port),
        _ => render_nginx(&service_name, input.port),
    };
    Ok(rendered)
}

fn service_name(app_key: &str) -> String {
    app_key
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn render_nginx(service_name: &str, port: u16) -> RenderedTemplate {
    RenderedTemplate {
        compose_content: format!(
            r#"services:
  {service_name}:
    image: nginx:1.27-alpine
    restart: unless-stopped
    ports:
      - "${{PUBLIC_PORT:-{port}}}:80"
    volumes:
      - ./html:/usr/share/nginx/html:ro
"#
        ),
        env_content: format!("PUBLIC_PORT={port}\n"),
    }
}

fn render_redis(service_name: &str, port: u16) -> RenderedTemplate {
    RenderedTemplate {
        compose_content: format!(
            r#"services:
  {service_name}:
    image: redis:7-alpine
    restart: unless-stopped
    command: ["redis-server", "--appendonly", "yes"]
    ports:
      - "${{REDIS_PORT:-{port}}}:6379"
    volumes:
      - ./data:/data
"#
        ),
        env_content: format!("REDIS_PORT={port}\n"),
    }
}

fn render_postgres(service_name: &str, port: u16) -> RenderedTemplate {
    RenderedTemplate {
        compose_content: format!(
            r#"services:
  {service_name}:
    image: postgres:16-alpine
    restart: unless-stopped
    ports:
      - "${{POSTGRES_PORT:-{port}}}:5432"
    environment:
      POSTGRES_DB: ${{POSTGRES_DB:-app}}
      POSTGRES_USER: ${{POSTGRES_USER:-app}}
      POSTGRES_PASSWORD: ${{POSTGRES_PASSWORD:-change-me}}
    volumes:
      - ./data:/var/lib/postgresql/data
"#
        ),
        env_content: format!(
            "POSTGRES_PORT={port}\nPOSTGRES_DB=app\nPOSTGRES_USER=app\nPOSTGRES_PASSWORD=change-me\n"
        ),
    }
}

fn render_caddy(service_name: &str, port: u16) -> RenderedTemplate {
    RenderedTemplate {
        compose_content: format!(
            r#"services:
  {service_name}:
    image: caddy:2-alpine
    restart: unless-stopped
    ports:
      - "${{PUBLIC_PORT:-{port}}}:80"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./data:/data
      - ./config:/config
"#
        ),
        env_content: format!("PUBLIC_PORT={port}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_postgres_template_with_env_and_port() {
        let rendered = render_compose_template(RenderTemplateInput {
            template_key: "postgres-single",
            app_key: "pg-main",
            port: 15432,
        })
        .expect("render template");

        assert!(rendered.compose_content.contains("postgres:16-alpine"));
        assert!(
            rendered
                .compose_content
                .contains("${POSTGRES_PORT:-15432}:5432")
        );
        assert!(rendered.env_content.contains("POSTGRES_PASSWORD=change-me"));
    }

    #[test]
    fn rejects_unknown_template_key() {
        let err = render_compose_template(RenderTemplateInput {
            template_key: "unknown",
            app_key: "demo",
            port: 8080,
        })
        .expect_err("unknown template should fail");

        assert_eq!(err.message(), "模板不存在");
    }
}
