use crate::runtimefs::DeployScriptSet;

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
    pub deploy_scripts: DeployScriptSet,
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
            description: "带密码、持久化、内存上限、健康检查和日志轮转的单节点 Redis。",
            image: "redis:7-alpine",
            default_port: 6379,
            env_hint: "REDIS_PASSWORD=change-me",
        },
        ComposeTemplate {
            key: "postgres-single",
            name: "PostgreSQL 单节点",
            description: "内置初始化账号和数据卷，仅建议开发、演示或临时验证使用；正式环境优先使用云 RDS。",
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
        ComposeTemplate {
            key: "loki-single",
            name: "Loki 单节点",
            description: "单机运行日志存储，适合配合 Alloy 采集 Docker stdout JSON 日志。",
            image: "grafana/loki:3.4.2",
            default_port: 3100,
            env_hint: "LOKI_PORT=3100",
        },
        ComposeTemplate {
            key: "alloy-docker-logs",
            name: "Alloy Docker 日志采集",
            description: "采集带 qfy_logs_enabled=true 标签的 Docker 容器日志并推送到 Loki。",
            image: "grafana/alloy:v1.6.1",
            default_port: 12345,
            env_hint: "LOKI_PUSH_URL=http://host.docker.internal:3100/loki/api/v1/push",
        },
        ComposeTemplate {
            key: "nats-jetstream",
            name: "NATS JetStream",
            description: "单节点 NATS JetStream，适合 Outbox 触发、异步任务和轻量消息消费。",
            image: "nats:2.10-alpine",
            default_port: 4222,
            env_hint: "NATS_PASSWORD=change-me",
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
        "loki-single" => render_loki(&service_name, input.port),
        "alloy-docker-logs" => render_alloy(&service_name, input.port),
        "nats-jetstream" => render_nats(&service_name, input.port),
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
        deploy_scripts: DeployScriptSet::default(),
    }
}

fn render_redis(service_name: &str, port: u16) -> RenderedTemplate {
    let compose_content = r#"services:
  {service_name}:
    image: ${REDIS_IMAGE:-redis:7-alpine}
    restart: unless-stopped
    command:
      - "redis-server"
      - "--appendonly"
      - "yes"
      - "--requirepass"
      - "${REDIS_PASSWORD:?set REDIS_PASSWORD}"
      - "--maxmemory"
      - "${REDIS_MAXMEMORY:-1024mb}"
      - "--maxmemory-policy"
      - "${REDIS_MAXMEMORY_POLICY:-allkeys-lru}"
      - "--maxclients"
      - "${REDIS_MAXCLIENTS:-10000}"
    environment:
      REDIS_PASSWORD: ${REDIS_PASSWORD:?set REDIS_PASSWORD}
      TZ: ${TZ:-Asia/Shanghai}
    ports:
      - "${REDIS_BIND_IP:-127.0.0.1}:${REDIS_PORT:-{port}}:6379"
    volumes:
      - ./data:/data
    healthcheck:
      test: ["CMD-SHELL", "redis-cli -a \"$${REDIS_PASSWORD}\" ping | grep PONG"]
      interval: 10s
      timeout: 5s
      retries: 20
    logging:
      driver: json-file
      options:
        max-size: "${DOCKER_LOG_MAX_SIZE:-200m}"
        max-file: "${DOCKER_LOG_MAX_FILE:-5}"
"#
    .replace("{service_name}", service_name)
    .replace("{port}", &port.to_string());
    RenderedTemplate {
        compose_content,
        env_content: format!(
            "REDIS_IMAGE=redis:7-alpine\nREDIS_BIND_IP=127.0.0.1\nREDIS_PORT={port}\nREDIS_PASSWORD=change-me\nREDIS_MAXMEMORY=1024mb\nREDIS_MAXMEMORY_POLICY=allkeys-lru\nREDIS_MAXCLIENTS=10000\nDOCKER_LOG_MAX_SIZE=200m\nDOCKER_LOG_MAX_FILE=5\n"
        ),
        deploy_scripts: DeployScriptSet {
            pre_deploy: "set -eu\nmkdir -p data\n".to_owned(),
            ..DeployScriptSet::default()
        },
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
        deploy_scripts: DeployScriptSet::default(),
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
        deploy_scripts: DeployScriptSet::default(),
    }
}

fn render_loki(project_name: &str, port: u16) -> RenderedTemplate {
    let compose_content = r#"name: {project_name}

services:
  loki:
    image: ${LOKI_IMAGE:-grafana/loki:3.4.2}
    restart: unless-stopped
    user: "${LOKI_RUN_USER:-0:0}"
    entrypoint: ["/bin/sh", "-lc"]
    command: |
      cat > /tmp/loki-config.yml <<'EOF'
      auth_enabled: false
      server:
        http_listen_port: 3100
        grpc_listen_port: 9095
      common:
        instance_addr: 127.0.0.1
        path_prefix: /loki
        storage:
          filesystem:
            chunks_directory: /loki/chunks
            rules_directory: /loki/rules
        replication_factor: 1
        ring:
          kvstore:
            store: inmemory
      query_range:
        results_cache:
          cache:
            embedded_cache:
              enabled: true
              max_size_mb: 128
      schema_config:
        configs:
          - from: 2024-04-01
            store: tsdb
            object_store: filesystem
            schema: v13
            index:
              prefix: index_
              period: 24h
      limits_config:
        retention_period: ${LOKI_RETENTION_PERIOD:-168h}
        allow_structured_metadata: true
        volume_enabled: true
      compactor:
        working_directory: /loki/compactor
        retention_enabled: true
        delete_request_store: filesystem
      EOF
      exec /usr/bin/loki -config.file=/tmp/loki-config.yml -config.expand-env=true
    ports:
      - "${LOKI_BIND_IP:-127.0.0.1}:${LOKI_PORT:-{port}}:3100"
    volumes:
      - ./data/loki:/loki
    healthcheck:
      test: ["CMD-SHELL", "wget -qO- http://127.0.0.1:3100/ready >/dev/null 2>&1 || exit 1"]
      interval: 10s
      timeout: 5s
      retries: 20
    logging:
      driver: json-file
      options:
        max-size: "${DOCKER_LOG_MAX_SIZE:-200m}"
        max-file: "${DOCKER_LOG_MAX_FILE:-5}"
"#
    .replace("{project_name}", project_name)
    .replace("{port}", &port.to_string());

    RenderedTemplate {
        compose_content,
        env_content: format!(
            "LOKI_IMAGE=grafana/loki:3.4.2\nLOKI_BIND_IP=127.0.0.1\nLOKI_PORT={port}\nLOKI_RETENTION_PERIOD=168h\nLOKI_RUN_USER=0:0\nDOCKER_LOG_MAX_SIZE=200m\nDOCKER_LOG_MAX_FILE=5\n"
        ),
        deploy_scripts: DeployScriptSet {
            pre_deploy: "set -eu\nmkdir -p data/loki\n".to_owned(),
            ..DeployScriptSet::default()
        },
    }
}

fn render_alloy(project_name: &str, port: u16) -> RenderedTemplate {
    let compose_content = r#"name: {project_name}

services:
  alloy:
    image: ${ALLOY_IMAGE:-grafana/alloy:v1.6.1}
    restart: unless-stopped
    entrypoint: ["/bin/sh", "-lc"]
    command: |
      cat > /tmp/config.alloy <<'EOF'
      discovery.docker "containers" {
        host = "unix:///var/run/docker.sock"
      }

      discovery.relabel "docker_logs" {
        targets = discovery.docker.containers.targets

        rule {
          source_labels = ["__meta_docker_container_label_qfy_logs_enabled"]
          regex         = "true"
          action        = "keep"
        }

        rule {
          source_labels = ["__meta_docker_container_label_qfy_project"]
          target_label  = "project"
        }

        rule {
          source_labels = ["__meta_docker_container_label_qfy_env"]
          target_label  = "env"
        }

        rule {
          source_labels = ["__meta_docker_container_label_qfy_service"]
          target_label  = "service"
        }

        rule {
          source_labels = ["__meta_docker_container_name"]
          regex         = "/(.*)"
          target_label  = "container"
        }
      }

      loki.source.docker "docker_logs" {
        host       = "unix:///var/run/docker.sock"
        targets    = discovery.relabel.docker_logs.output
        forward_to = [loki.process.docker_logs.receiver]
      }

      loki.process "docker_logs" {
        stage.json {
          expressions = {
            level = "level",
          }
        }

        stage.labels {
          values = {
            level = "",
          }
        }

        forward_to = [loki.write.default.receiver]
      }

      loki.write "default" {
        endpoint {
          url          = sys.env("LOKI_PUSH_URL")
          bearer_token = sys.env("LOKI_GATEWAY_TOKEN")
        }
      }
      EOF
      exec /bin/alloy run --server.http.listen-addr=0.0.0.0:12345 --storage.path=/var/lib/alloy/data /tmp/config.alloy
    environment:
      LOKI_PUSH_URL: ${LOKI_PUSH_URL:-http://host.docker.internal:3100/loki/api/v1/push}
      LOKI_GATEWAY_TOKEN: ${LOKI_GATEWAY_TOKEN:-}
    ports:
      - "${ALLOY_BIND_IP:-127.0.0.1}:${ALLOY_HTTP_PORT:-{port}}:12345"
    extra_hosts:
      - "host.docker.internal:host-gateway"
    volumes:
      - ./data/alloy:/var/lib/alloy/data
      - /var/run/docker.sock:/var/run/docker.sock:ro
    logging:
      driver: json-file
      options:
        max-size: "${DOCKER_LOG_MAX_SIZE:-100m}"
        max-file: "${DOCKER_LOG_MAX_FILE:-3}"
"#
    .replace("{project_name}", project_name)
    .replace("{port}", &port.to_string());

    RenderedTemplate {
        compose_content,
        env_content: format!(
            "ALLOY_IMAGE=grafana/alloy:v1.6.1\nALLOY_BIND_IP=127.0.0.1\nALLOY_HTTP_PORT={port}\nLOKI_PUSH_URL=http://host.docker.internal:3100/loki/api/v1/push\nLOKI_GATEWAY_TOKEN=\nDOCKER_LOG_MAX_SIZE=100m\nDOCKER_LOG_MAX_FILE=3\n"
        ),
        deploy_scripts: DeployScriptSet {
            pre_deploy: "set -eu\nmkdir -p data/alloy\n".to_owned(),
            ..DeployScriptSet::default()
        },
    }
}

fn render_nats(project_name: &str, port: u16) -> RenderedTemplate {
    let compose_content = r#"name: {project_name}

services:
  nats:
    image: ${NATS_IMAGE:-nats:2.10-alpine}
    restart: unless-stopped
    command:
      - "nats-server"
      - "--jetstream"
      - "--store_dir"
      - "/data/nats/jetstream"
      - "--user"
      - "${NATS_USER:-app}"
      - "--pass"
      - "${NATS_PASSWORD:?set NATS_PASSWORD}"
      - "--http_port"
      - "8222"
    environment:
      TZ: ${TZ:-Asia/Shanghai}
      NATS_USER: ${NATS_USER:-app}
      NATS_PASSWORD: ${NATS_PASSWORD:?set NATS_PASSWORD}
    ports:
      - "${NATS_BIND_IP:-127.0.0.1}:${NATS_PORT:-{port}}:4222"
      - "${NATS_MONITOR_BIND_IP:-127.0.0.1}:${NATS_MONITOR_PORT:-8222}:8222"
    volumes:
      - ./data/nats:/data/nats
    healthcheck:
      test: ["CMD-SHELL", "wget -qO- http://127.0.0.1:8222/varz >/dev/null 2>&1 || exit 1"]
      interval: 10s
      timeout: 3s
      retries: 12
      start_period: 20s
    logging:
      driver: json-file
      options:
        max-size: "${DOCKER_LOG_MAX_SIZE:-100m}"
        max-file: "${DOCKER_LOG_MAX_FILE:-3}"
"#
    .replace("{project_name}", project_name)
    .replace("{port}", &port.to_string());

    RenderedTemplate {
        compose_content,
        env_content: format!(
            "NATS_IMAGE=nats:2.10-alpine\nNATS_BIND_IP=127.0.0.1\nNATS_PORT={port}\nNATS_MONITOR_BIND_IP=127.0.0.1\nNATS_MONITOR_PORT=8222\nNATS_USER=app\nNATS_PASSWORD=change-me\nDOCKER_LOG_MAX_SIZE=100m\nDOCKER_LOG_MAX_FILE=3\n"
        ),
        deploy_scripts: DeployScriptSet {
            pre_deploy: "set -eu\nmkdir -p data/nats\n".to_owned(),
            ..DeployScriptSet::default()
        },
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
    fn lists_templates_and_finds_by_key() {
        let templates = compose_templates();

        assert_eq!(templates.len(), 7);
        assert_eq!(
            template_by_key("redis-single")
                .expect("redis template")
                .image,
            "redis:7-alpine"
        );
        assert_eq!(
            template_by_key("loki-single")
                .expect("loki template")
                .default_port,
            3100
        );
        assert_eq!(
            template_by_key("alloy-docker-logs")
                .expect("alloy template")
                .image,
            "grafana/alloy:v1.6.1"
        );
        assert_eq!(
            template_by_key("nats-jetstream")
                .expect("nats template")
                .default_port,
            4222
        );
        assert!(template_by_key("missing").is_none());
    }

    #[test]
    fn renders_compose_templates_with_sanitized_service_names() {
        let cases = [
            (
                "nginx-static",
                "Web App",
                "web-app",
                "nginx:1.27-alpine",
                "PUBLIC_PORT=8081\n",
            ),
            (
                "redis-single",
                "Cache@Prod",
                "cache-prod",
                "redis:7-alpine",
                "REDIS_PORT=6380",
            ),
            (
                "caddy-gateway",
                "Edge.Gateway",
                "edge-gateway",
                "caddy:2-alpine",
                "PUBLIC_PORT=8088\n",
            ),
        ];

        for (template_key, app_key, service, image, env) in cases {
            let rendered = render_compose_template(RenderTemplateInput {
                template_key,
                app_key,
                port: env
                    .trim()
                    .rsplit_once('=')
                    .expect("env port")
                    .1
                    .parse()
                    .expect("port"),
            })
            .expect("render template");

            assert!(rendered.compose_content.contains(&format!("  {service}:")));
            assert!(rendered.compose_content.contains(image));
            assert!(rendered.env_content.contains(env));
        }
    }

    #[test]
    fn renders_runtime_infrastructure_templates() {
        let redis = render_compose_template(RenderTemplateInput {
            template_key: "redis-single",
            app_key: "redis-prod",
            port: 16379,
        })
        .expect("render redis");
        assert!(redis.compose_content.contains("--requirepass"));
        assert!(redis.compose_content.contains("--maxclients"));
        assert!(redis.env_content.contains("REDIS_MAXCLIENTS=10000"));
        assert!(
            redis
                .compose_content
                .contains("${REDIS_BIND_IP:-127.0.0.1}:${REDIS_PORT:-16379}:6379")
        );
        assert!(
            redis
                .compose_content
                .contains("max-size: \"${DOCKER_LOG_MAX_SIZE:-200m}\"")
        );
        assert!(redis.deploy_scripts.pre_deploy.contains("mkdir -p data"));

        let loki = render_compose_template(RenderTemplateInput {
            template_key: "loki-single",
            app_key: "qfy-sc-loki",
            port: 3100,
        })
        .expect("render loki");
        assert!(loki.compose_content.contains("grafana/loki:3.4.2"));
        assert!(
            loki.compose_content
                .contains("retention_period: ${LOKI_RETENTION_PERIOD:-168h}")
        );
        assert!(loki.compose_content.contains("/ready"));
        assert!(loki.env_content.contains("LOKI_BIND_IP=127.0.0.1"));
        assert!(loki.deploy_scripts.pre_deploy.contains("data/loki"));

        let alloy = render_compose_template(RenderTemplateInput {
            template_key: "alloy-docker-logs",
            app_key: "qfy-sc-alloy",
            port: 12345,
        })
        .expect("render alloy");
        assert!(alloy.compose_content.contains("qfy_logs_enabled"));
        assert!(
            alloy
                .compose_content
                .contains("/var/run/docker.sock:/var/run/docker.sock:ro")
        );
        assert!(
            alloy
                .compose_content
                .contains("host.docker.internal:host-gateway")
        );
        assert!(
            alloy
                .env_content
                .contains("LOKI_PUSH_URL=http://host.docker.internal:3100/loki/api/v1/push")
        );

        let nats = render_compose_template(RenderTemplateInput {
            template_key: "nats-jetstream",
            app_key: "qfy-sc-nats",
            port: 4222,
        })
        .expect("render nats");
        assert!(nats.compose_content.contains("--jetstream"));
        assert!(
            nats.compose_content
                .contains("${NATS_PASSWORD:?set NATS_PASSWORD}")
        );
        assert!(
            nats.compose_content
                .contains("${NATS_MONITOR_BIND_IP:-127.0.0.1}:${NATS_MONITOR_PORT:-8222}:8222")
        );
        assert!(nats.deploy_scripts.pre_deploy.contains("data/nats"));
    }

    #[test]
    fn rejects_empty_app_key_and_zero_port() {
        let empty_key = render_compose_template(RenderTemplateInput {
            template_key: "nginx-static",
            app_key: " ",
            port: 8080,
        })
        .expect_err("empty app key should fail");
        assert!(matches!(empty_key, CatalogError::InvalidInput(_)));

        let zero_port = render_compose_template(RenderTemplateInput {
            template_key: "nginx-static",
            app_key: "demo",
            port: 0,
        })
        .expect_err("zero port should fail");
        assert!(matches!(zero_port, CatalogError::InvalidInput(_)));
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
