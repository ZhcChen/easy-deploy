# Compose 发布单元模板契约

本文档定义业务仓库接入 easy-deploy 时推荐提交的模板结构。平台只消费这套通用契约，不内置任何业务项目专属逻辑。

## 适用目标

业务仓库负责构建版本包和维护可提交模板，easy-deploy 负责：

- 可视化维护目标节点、Compose、环境变量、部署脚本和健康检查。
- 接收页面或 OpenAPI 投递的版本包。
- 按应用发布模式立即入队、等待手动发布或定时发布。
- 串行执行发布任务并记录阶段、步骤和原始输出。

OpenAPI 只做版本包投递，不创建应用、不更新配置、不触发部署控制面。

## 推荐目录

业务仓库可以按环境维护模板目录：

```text
deploy/easy-deploy/
  testing/
    <app-key>/
      app.yaml.example
      compose.yaml.example
      .env.example
      scripts/
        00-preflight.sh
        20-migrate.sh
        90-healthcheck.sh
  production/
    <app-key>/
      app.yaml.example
      compose.yaml.example
      .env.example
      scripts/
        00-preflight.sh
        20-migrate.sh
        90-healthcheck.sh
```

复杂系统可以拆成多个发布单元，例如数据库、Redis、NATS、Loki、Alloy、Gateway、Backend、Worker 和前端静态站点。每个发布单元在 easy-deploy 中对应一个应用。

## app.yaml.example

`app.yaml.example` 只描述部署单元的元信息和默认健康检查，不放真实密钥。

```yaml
app_key: "orders-api-test"
name: "订单 API 测试环境"
environment: "test"
app_type: "compose"
release_source: "package_upload"
deploy_strategy: "rolling_stop_on_failure"
health_check_kind: "http"
health_endpoint: "http://127.0.0.1:8080/healthz"
health_timeout_secs: 5
health_expected_status: 200
notes:
  - "发布前执行 migration 脚本。"
```

字段约定：

- `app_key`：平台服务标识，必须和版本包名前缀一致。
- `environment`：当前固定为 `test` 或 `production`。
- `release_source`：`manual` 或 `package_upload`。
- `deploy_strategy`：节点滚动策略，当前支持 `rolling_stop_on_failure` 和 `rolling_continue`。
- `health_*`：部署后健康检查默认值。

基础设施类应用通常使用 `release_source: manual`，例如 Redis、PostgreSQL、NATS、Loki、Alloy 和 Gateway。

业务运行类应用通常使用 `release_source: package_upload`，由业务项目或 CI 调用 OpenAPI 投递版本包。

## compose.yaml.example

`compose.yaml.example` 表达运行拓扑。平台不在模板里推断业务语义，只负责保存、渲染和执行。

建议：

- 明确 `restart: unless-stopped`。
- 配置 Docker 日志轮转，避免宿主机日志无限增长。
- 需要运行日志采集时，给容器加明确 label，例如项目、环境和服务名。
- 测试和正式环境尽量同构，只通过 `.env`、域名、资源上限和目标节点表达差异。

示例：

```yaml
name: orders-api-test

services:
  api:
    image: ${ORDERS_API_IMAGE:?set ORDERS_API_IMAGE}
    restart: unless-stopped
    environment:
      APP_ENV: ${APP_ENV:-testing}
      APP_DATABASE_URL: ${APP_DATABASE_URL:?set APP_DATABASE_URL}
    ports:
      - "${API_BIND_IP:-127.0.0.1}:${API_PORT:-8080}:8080"
    labels:
      qfy_logs_enabled: "true"
      qfy_project: "orders"
      qfy_env: "testing"
      qfy_service: "api"
    healthcheck:
      test: ["CMD-SHELL", "wget -qO- http://127.0.0.1:8080/healthz >/dev/null 2>&1 || exit 1"]
      interval: 10s
      timeout: 5s
      retries: 20
    logging:
      driver: json-file
      options:
        max-size: "${DOCKER_LOG_MAX_SIZE:-100m}"
        max-file: "${DOCKER_LOG_MAX_FILE:-3}"
```

## .env.example

`.env.example` 只提交变量名、默认值和占位，不提交真实密钥。

```dotenv
APP_ENV=testing
API_BIND_IP=127.0.0.1
API_PORT=8080
ORDERS_API_IMAGE=orders-api-test:latest
APP_DATABASE_URL=
APP_SECURITY_MASTER_KEY=
```

敏感值必须在 easy-deploy 后台配置或目标服务器安全目录中维护，不写入仓库、版本包、构建日志或通知内容。

## scripts

脚本是发布流程的业务扩展点。平台只按用户配置执行脚本并记录输出，不理解脚本里的业务含义。

推荐把脚本映射到 easy-deploy 的固定阶段槽位：

- `pre_deploy`：预检、准备目录、检查外部依赖。
- `deploy`：加载镜像、复制静态资源、执行 `docker compose up -d`。
- `post_deploy`：migration、seed、MQ repair、缓存预热等。
- `switch_traffic`：Compose 蓝绿发布时的切流动作。
- `cleanup`：清理旧目录、旧镜像或临时文件。

脚本必须满足：

- 使用 `set -eu` 或等价失败策略。
- 输出对排障有用的信息。
- 失败时返回非 0 状态。
- 不在脚本里写死 easy-deploy 平台内部路径。

平台执行脚本时会注入发布上下文，脚本应优先读取这些变量：

```text
ED_APP_ID
ED_APP_KEY
ED_APP_NAME
ED_ENVIRONMENT
ED_APP_DIR
ED_RELEASE_ID
ED_RELEASE_VERSION
ED_RELEASE_DIR
ED_RELEASE_BUNDLE_DIR
ED_RELEASE_RENDER_DIR
ED_CURRENT_LINK
ED_TARGET_NODE_KEY
ED_TARGET_NODE_NAME
ED_COMPOSE_STRATEGY
ED_ACTIVE_SLOT
ED_STANDBY_SLOT
```

## 发布模式

`release_source=manual`：

- 不需要版本包。
- 适合基础设施和网关。
- 用户在后台修改 Compose/env/scripts 后，手动执行当前配置发布。

`release_source=package_upload`：

- 业务项目通过页面或 OpenAPI 上传版本包。
- 包名必须符合 `{service_key}_version_{x_y_z}.tar.gz`。
- 上传后平台登记发布版本，并根据应用设置决定立即入队、等待手动发布或定时发布。

## 测试与正式同构

测试环境和正式环境应尽量共用同一套目录形态：

- 相同的发布单元拆分。
- 相同的脚本阶段。
- 相同的 Compose 服务结构。
- 不同的 `.env`、域名、资源限制、目标节点和密钥。

这样测试环境验证通过后，正式环境只需要复制配置结构并填入正式环境差异，而不是重新设计部署流程。

## 明确非目标

easy-deploy 不做：

- Git tag 拉取、源码构建或业务镜像构建。
- 业务数据库内容回滚。
- qfy-sc、订单系统、商城等项目的专属逻辑分支。
- 外部 OpenAPI 控制应用创建、配置修改、节点读取或任务轮询。

这些边界能保持平台简单，也能让业务项目和 AI 接入时只关注“构建版本包并投递到正确应用”。
