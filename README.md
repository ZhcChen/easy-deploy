# easy-deploy

一个使用 Rust 构建的轻量部署平台。当前仓库先初始化为多模块 Cargo workspace，首期模块包括：

- `api`：主服务模块，承载 Axum Web/API、SQLite 迁移、服务端 HTML 模板和静态资源。
- `e2e`：验收测试模块，用真实 HTTP 服务启动方式验证主服务基础行为。

当前已接入：

- 账号登录、初始化超级管理员、RBAC、会话管理、审计日志。
- 内存会话 + access / refresh 双 opaque token，通过 HttpOnly Cookie 保存；服务重启后需要重新登录，生产 HTTPS 可开启 Secure Cookie。
- 总览仪表盘：应用数、服务数、节点数、运行任务、最近应用、节点和任务均从当前数据库读取。
- 节点管理基础模型，默认本机节点，支持新增 SSH 节点、本机 Docker 探测和节点执行能力展示。
- 应用管理基础模型，支持创建 Docker Compose / 二进制应用并绑定目标节点。
- 服务索引：从 Compose 应用的 `compose.yaml` 自动派生 service 列表，展示镜像、端口、实例数、目标节点、健康检查和状态，并支持查看单个 service 最近 200 行日志。
- 应用 runtimefs：创建应用时写入 `data_dir/apps/<app_key>/compose.yaml`、`.env`、`.easy-deploy/app.yaml`。
- 应用详情页：读取和保存 runtimefs 中的 `compose.yaml` 与 `.env`，展示配置快照并支持恢复快照，同时展示目标节点运行状态和最近部署历史。
- 二进制直部署第一阶段：支持登记已有二进制制品路径、版本、启动参数、运行用户和 systemd unit，并通过 systemctl restart/stop 进入统一任务系统。
- 健康检查配置：支持关闭检查、HTTP GET、TCP 连接、Compose 容器运行状态和 systemd active 检查。
- 模板创建流：内置 Nginx 静态站点、Redis、PostgreSQL、Caddy 网关 Compose 模板，创建后直接进入普通应用详情页。
- 本机 Docker Compose 执行器：封装 `docker compose config/up/down/restart/logs`，详情页已接入配置校验和最近日志入口，并兼容过滤旧 Compose 文件的顶层 `version:`。
- 任务系统：`operation_tasks`、`operation_task_logs`、`deployment_runs`，Compose 部署/停止/重启会入队后台 worker，先执行 Docker daemon、本地目录/磁盘/端口与 `docker compose config` 预检，部署/重启命令成功后继续执行健康检查，再记录为可追踪任务，并提供任务详情页查看分段日志、重试失败 Compose 任务、取消等待中的任务。

## 技术栈

- `axum`：HTTP 路由与服务入口
- `tokio`：异步运行时
- `sqlx`：SQLite 持久化与迁移
- `askama`：服务端 HTML 模板
- `clap`：命令行参数与环境变量
- `fs2`：部署目录磁盘空间检查
- `reqwest`：HTTP 健康检查
- `serde_yaml`：Compose YAML 解析与主机端口识别
- `tracing`：结构化日志

## 本地运行

```bash
cargo run -p api -- --bind 127.0.0.1:9066 --database-url sqlite://easy-deploy.db
```

可用环境变量：

- `EASY_DEPLOY_BIND`
- `EASY_DEPLOY_DATABASE_URL`
- `EASY_DEPLOY_DATA_DIR`
- `EASY_DEPLOY_COOKIE_SECURE`：生产 HTTPS 场景建议设为 `true`

## 测试

```bash
cargo test --workspace
```

`e2e` 模块会启动一个监听随机本地端口的真实 `api` 服务，并验证：

- `GET /healthz` 返回 `ok`
- 初始化管理员、登录、refresh、退出和改密。
- RBAC 菜单过滤、403 授权拦截、账号禁用。
- 账号、角色、会话、审计页面主流程。
- 节点默认数据、新增节点、只读用户不可管理节点。
- 应用创建、目标节点绑定、只读用户不可创建应用。
- 总览真实数据、应用详情配置读取、配置保存、目标节点运行状态、配置快照展示与恢复、健康检查配置、服务索引派生、服务维度日志查看、只读用户不可保存配置或恢复快照。
- 模板页展示、从 Redis 模板创建应用、只读用户不可从模板创建应用。
- Compose 执行器命令参数单元测试，以及只读用户不可触发 Compose 操作。
- 二进制应用创建、二进制配置保存、systemd restart 任务、systemd active 健康检查和运行状态更新。
- Compose 部署任务创建、后台异步执行、应用详情部署历史、部署前预检失败、部署后健康检查、本地目录/磁盘/端口预检、旧版 `version:` 过滤、任务列表/详情页展示、失败任务重试、等待任务取消和权限拦截。
