---
date: 2026-06-01
topic: core-deployment-platform-modules
status: active
origin: user request on 2026-06-01
---

# Easy Deploy 核心部署平台模块规划

## 问题框架

Easy Deploy 的目标不是做一个“功能很多但难上手”的部署系统，而是做一个默认路径清晰、关键能力不缺失的轻量部署平台：

- 用户能用 Docker Compose 快速创建、部署和维护多服务应用。
- Go 等可打包为可执行文件的服务，可以选择直接二进制部署，并支持单机尽量不停机更新。
- 平台后续需要支持多节点和目标服务分布式部署，但第一阶段不能为了集群调度牺牲易用性。
- 当前 UI 已经明确出主导航：总览、应用、服务、节点、部署任务、模板、制品、设置。后端模块应围绕这些导航形成稳定边界。

参考项目 `tmp/mini-deploy` 已经验证过很多单机部署能力，但它的问题也很明显：配置编辑、模板、发布物、部署历史、运行时状态逐渐混在一起，导致产品不够简单。Easy Deploy 应吸收它成熟的运行时做法，避免照搬它过重的页面模型。

## 产品原则

1. 文件是真源，数据库做索引和历史。
   页面表单、向导和模板只负责生成或更新真实配置文件。SQLite 保存应用索引、节点索引、任务、状态、发布历史和快照，不重新发明一套隐藏配置源。

2. 应用是统一对象。
   模板应用、自定义 Compose 应用、二进制应用都进入同一应用列表、同一部署历史、同一任务系统。模板只是创建时的预填方式。

3. 默认简单，高级可见但不挡路。
   新建应用先问“从模板创建 / 自定义 Compose / 上传二进制”，而不是直接暴露大块配置编辑器。高级配置保留原始文件编辑入口。

4. 部署成功不等于命令成功。
   每次部署需要区分命令执行、健康检查、运行时状态回写三个阶段。

5. 回滚语义必须明确。
   二进制应用回滚的是“发布物版本 + 当时部署定义”。Compose/模板应用第一阶段只回滚“部署配置快照”，不承诺恢复数据目录。

## 参考项目可复用点

### 值得直接迁移的设计

- `tmp/mini-deploy/api/internal/runtimefs/app_config.go`
  - 应用目录落盘为 `compose.yaml`、`.env`、`.mini-deploy/app.yaml`、可选 `.mini-deploy/deploy.sh`。
  - 这个“文件为真源”的结构应作为 Easy Deploy 的运行时配置基础。

- `tmp/mini-deploy/api/internal/runtimefs/compose_runtime.go`
  - 执行 `docker compose` 前生成临时 compose 文件，并过滤顶层 `version:`。
  - Easy Deploy 应保留这个兼容处理，避免用户粘贴旧 compose 文件时出现无谓警告或失败。

- `tmp/mini-deploy/api/internal/deploy/preflight.go`
  - 部署前检查 Docker CLI、Docker daemon、docker compose、部署目录可写性、磁盘空间、端口占用。
  - Easy Deploy 应把预检作为部署任务的第一阶段，并在 UI 里明确展示阻塞项和警告项。

- `tmp/mini-deploy/api/internal/deploy/service.go`
  - 应用级锁、部署任务、部署历史、日志读取、部署后健康检查这些边界值得保留。
  - 但实现时应拆成 Rust 中更小的模块，避免单个 service 承载过多分支。

- `tmp/mini-deploy/api/internal/deploy/systemd_unit.go`
  - `restart` 和 `blue_green` 两种 systemd 二进制发布策略可以复用为 Rust 版设计来源。
  - 当前 Rust 版已经接入 systemd restart 和 Blue/Green 切流路径，后续重点转向失败回退策略、节点级安装引导和更细的部署差异展示。

- `tmp/mini-deploy/api/internal/catalog/templates.go`
  - `nginx-static`、`redis-single`、`postgres-single`、`caddy-gateway` 这些 Compose 模板可以作为第一批内置模板。

### 需要调整后再用的设计

- 参考项目后期引入 `scripted_v2`，把部署脚本作为统一入口。这个方向有价值，但 Easy Deploy 第一阶段不应让所有用户先理解脚本。
  - Compose 应用默认展示结构化入口和原始 compose 编辑。
  - 二进制应用默认展示发布物、启动命令、端口、健康检查。
  - `deploy.sh` 可以作为高级模式或内部生成物。

- 参考项目的模板版本锁定规则适合 PostgreSQL/Redis，但第一阶段可以先不做“模板版本升级流程”。需要先保证创建、部署、健康检查和配置快照闭环。

## 目标模块

### 1. 核心运行时与配置文件模块

Rust 模块建议：

- `api/src/domain/`
  - 纯业务类型：App、Service、Node、Artifact、Deployment、Template、HealthCheck。
- `api/src/runtimefs/`
  - 负责读写应用运行时目录。
- `api/src/deploy/`
  - 负责执行部署动作、预检、健康检查、日志抓取。
- `api/src/catalog/`
  - 内置模板注册中心。
- `api/src/tasks/`
  - 操作任务、任务日志和任务状态。

应用目录建议：

```text
<data_dir>/apps/<app-name>/
├── compose.yaml
├── .env
├── releases/
│   └── <release-id>/
└── .easy-deploy/
    ├── app.yaml
    └── deploy.sh
```

说明：

- Docker Compose 应用必须有 `compose.yaml`，可选 `.env`。
- 二进制应用必须有 `.easy-deploy/app.yaml`，发布物解压或复制到 `releases/<release-id>/`。
- `.easy-deploy/deploy.sh` 第一阶段可以由系统生成，后续再开放高级编辑。
  当前实现先通过 runtimefs 生成 compose、systemd unit/env、release/current 和 app.yaml，暂不暴露 deploy.sh 高级编辑。

### 2. 应用模块

职责：

- 管理应用基本信息：名称、描述、类型、部署目录、目标节点、创建来源。
- 支持三种创建入口：
  - 从模板创建：Redis、PostgreSQL、Nginx、Caddy 等。
  - 自定义 Docker Compose：粘贴或上传 compose，选择目标节点。
  - 二进制服务：上传可执行文件或 tar.gz，填写启动命令、运行用户、端口和健康检查。
- 应用详情页拆成清晰区域：
  - 概览：当前版本、状态、目标节点、最近任务。
  - 服务：compose services 或 systemd service。
  - 配置：结构化表单 + 原始文件入口。
  - 发布物：二进制应用的版本列表。
  - 部署历史：任务、输出、健康检查、快照。

首批数据库表：

```text
apps
app_targets
app_runtime_states
app_config_snapshots
```

### 3. 服务模块

“服务”不是独立创建的顶级对象，而是应用里的可运行单元在平台中的索引视图：

- Compose 应用：从 `compose.yaml` 的 `services` 解析出服务列表。
- 二进制应用：默认一个 systemd 服务，后续可扩展多进程。
- 服务页提供按服务维度的状态、端口、健康检查、日志入口。

这样保留当前 UI 的“服务”导航，同时避免让用户先创建服务再创建应用，降低理解成本。

### 4. 节点模块

节点是部署目标。第一阶段建议支持：

- `local` 节点：当前 Easy Deploy 所在机器，默认存在。
- `ssh` 节点：远程 Linux 主机，保存连接配置、标签、区域、状态。

节点能力分层：

- 节点注册：名称、地址、SSH 用户、认证方式、工作目录、标签。
- 节点探测：OS、架构、Docker、compose、systemd、磁盘空间。
- 节点执行：封装本地执行和 SSH 远程执行。
- 节点组件：Docker / Caddy / Nginx 安装与状态，作为节点能力展示，不与 Redis/PostgreSQL 这类应用模板混淆。

首批数据库表：

```text
nodes
node_capabilities
node_checks
```

### 5. 部署任务模块

所有耗时动作都进入统一任务系统：

- 部署应用
- 停止 / 重启应用
- 预检
- 上传发布物
- 节点探测
- 安装 Docker / Caddy / Nginx

任务模型：

```text
operation_tasks
operation_task_logs
deployment_runs
```

任务阶段建议：

1. queued
2. preflight
3. preparing_files
4. executing
5. healthchecking
6. completed / failed / canceled

第一阶段可以使用进程内 Tokio 后台任务队列。先不引入 Redis、NATS 或外部 worker，避免部署平台自己变得难部署。

### 6. 模板模块

模板目标是降低上手成本，不是做应用市场。

第一批模板：

- Nginx 静态站点
- Redis 单节点
- PostgreSQL 单节点
- Caddy 网关

模板输出真实文件：

- `compose.yaml`
- `.env`
- `.easy-deploy/app.yaml`

结构化参数第一阶段只覆盖高频字段：

- 镜像版本
- 映射端口
- 数据目录
- 初始账号/密码
- Redis appendonly 和额外参数

必须保留“原始 compose 编辑”入口，避免结构化表单覆盖用户高级配置。

### 7. 制品模块

制品主要服务二进制部署：

- 上传二进制文件或 tar.gz。
- 计算 sha256、大小、原始文件名。
- 绑定应用和版本号。
- 解压 tar.gz 时允许配置入口文件。
- 根据保留策略清理旧版本。

首批数据库表：

```text
artifacts
app_releases
```

二进制部署策略：

- `restart`：生成单个 systemd unit，替换 current 指向或直接更新 unit 后重启。
- `blue_green`：已支持生成 blue/green 两个 unit，健康检查通过后切 Caddy/Nginx 反代；后续补失败回退提示和专用回滚入口。

### 8. 健康检查与日志模块

健康检查模式：

- none
- HTTP GET
- TCP connect
- Docker container running
- systemd active

日志入口：

- Compose：`docker compose logs --tail N --no-color`
- Systemd：`journalctl -u <unit> -n N --no-pager`
- 任务日志：平台自己记录的阶段输出

第一阶段只做“最近日志”，不做实时 websocket 流。实时日志可以后续加。

### 9. 设置模块

设置页先保持克制：

- 数据目录
- 默认应用部署目录模板
- 默认节点工作目录
- 命令超时
- 制品保留默认值
- 面板版本和运行信息
- `settings.view` / `settings.update` 权限边界

当前实现已经把默认应用部署目录模板、默认节点工作目录、上传制品保留数持久化到 `platform_settings`，保存后会立即影响新建应用、新建模板应用、新建节点和下一次上传制品清理。命令超时仍作为启动参数展示，因为命令执行器在启动时构造，热更新会误导用户。

认证已经按用户要求提前纳入第一阶段：使用账号、角色、权限、会话和审计日志形成后台安全边界。当前实现采用内存会话、access + refresh 双 opaque token、HttpOnly Cookie、refresh token 轮换和会话强制下线；部署平台自身不依赖 Redis，服务重启后需要重新登录。后续部署、节点、应用和设置类操作都应继续接入同一套 RBAC 权限 key，而不是另做一套授权逻辑。

## 分阶段实现路线

### 阶段 1：单机 Compose 闭环

目标：用户能从模板或自定义 compose 创建应用，在本机部署、查看状态、看日志。

实现单元：

1. 数据模型与 runtimefs
   - 已新增 apps、nodes、app_targets、app_runtime_states、app_config_snapshots、node_checks。
   - 已新增 runtimefs，创建应用时会写入 `data_dir/apps/<app-key>/compose.yaml`、`.env`、`.easy-deploy/app.yaml`。
   - 已新增应用详情页，支持读取和保存 `compose.yaml`、`.env`，保存时追加 manual 配置快照。
   - 已在应用详情页展示最近 10 条配置快照，并支持按快照恢复当前 runtimefs 配置。
   - 已新增 operation_tasks、operation_task_logs、deployment_runs。
   - 已把 `app_runtime_states` 接入 Compose 任务生命周期，并在应用详情页展示每个目标节点的运行态、服务数和最近部署版本。
   - 已在应用详情页展示最近 10 条部署历史，并关联任务详情入口。
   - 已在部署确认页展示配置文件、runtime metadata、systemd unit/env、release/current、代理配置等部署前文件计划。
   - 待补 app.yaml 的结构化编辑和更细的内容级差异预览。

2. 本地节点和 Compose 执行器
   - 已默认创建 local 节点。
   - 已支持节点列表、新增 SSH 节点、本机 Docker CLI 探测和节点执行能力展示。
   - 已封装本机 `docker compose config/up/down/restart/logs` 执行器，并在应用详情页接入配置校验和最近日志入口。
   - 已把 up/down/restart 接入任务系统，并通过进程内 Tokio 后台队列顺序执行，页面请求只负责创建任务和入队。
   - 已在后台任务中接入 Docker daemon 与 `docker compose config` 预检，预检失败会阻断实际部署并写入任务失败摘要。
   - 已过滤 compose 顶层 `version:`，保留嵌套字段，避免旧 Compose 文件在新 CLI 中产生无意义警告。
   - 已对 Docker / Compose 预检错误做简短摘要，去掉常见噪声前缀。
   - 已补任务详情页，支持查看任务元信息、执行命令、摘要、退出码和分段任务日志。
   - 已支持失败 Compose 任务重试，并支持取消尚未开始执行的排队任务。

3. 预检和健康检查
   - 已检查 Docker daemon 与 Compose 配置有效性。
   - 已检查部署目录可写性和磁盘可用空间，端口占用作为部署前警告写入任务日志。
   - 已支持 none、HTTP GET、TCP connect、Compose 容器运行状态和 systemd active 健康检查。
   - 已在应用详情页提供健康检查配置入口，部署/重启命令成功后继续执行健康检查，失败时任务和应用状态都会标记为失败。
   - 待补按服务维度展示最近一次健康检查明细。

4. 模板创建流
   - 已内置 Nginx 静态站点、Redis 单节点、PostgreSQL 单节点、Caddy 网关。
   - 已支持从模板创建 Docker Compose 应用，生成 `compose.yaml` 和 `.env` 后进入普通应用详情页。

5. UI 页面落地
   - `/` 总览页已从静态 mock 改为读取真实应用、服务、节点和任务数据。
   - `/apps` 已接入真实应用列表和创建入口。
   - `/apps/:id` 已接入应用详情、配置保存、目标节点运行状态、配置快照展示/恢复、部署历史、Compose 校验、日志入口和部署动作。
   - `/services` 已接入真实服务索引，从 Compose 配置派生 service 名称、镜像、端口、实例数、目标节点、健康检查和状态。
   - `/services/:app_id/:service_name/logs` 已接入 Compose service 维度最近 200 行日志。
   - `/tasks` 已接入任务列表。
   - `/tasks/:id` 已接入任务详情、分段日志、失败重试和等待任务取消。
   - `/templates` 已接入模板列表和模板创建流。

验收场景：

- 从 Redis 模板创建应用，部署成功后任务状态为成功。
- 粘贴一个带 `version:` 的 compose 文件，保存时自动过滤并能成功执行。
- Docker 未安装时，预检阻塞部署并在任务日志中显示原因。
- 部署失败时，应用状态不被错误标记为健康。
- 能查看最近 200 行 compose 日志。

### 阶段 2：二进制直部署

目标：Go/Rust 等单文件服务可上传并由 systemd 管理。

实现单元：

1. 制品存储
   - 已新增 binary_artifacts 和 app_binary_configs。
   - 已支持登记已有 binary 路径、版本、启动参数、运行用户和 systemd unit。
   - 已支持上传 binary / tar.gz，记录 checksum、大小、原始文件名、入口文件和上传来源。
   - 已支持按平台设置清理旧上传制品，当前版本不会被清理。

2. systemd restart 策略
   - 已封装 systemctl restart/stop/is-active 执行器，并接入统一任务系统。
   - 已支持 systemd active 健康检查，成功后回写 app_runtime_states。
   - 已生成 unit、env file、release.yaml、current 指针和 app.yaml，并在部署时同步到目标节点。

3. 发布历史和回滚
   - 已把 binary_restart/binary_stop 写入 deployment_runs。
   - 已支持选择旧 release 激活并创建新的二进制重启任务。
   - 待补更明确的“回滚”按钮语义和失败回退提示。

验收场景：

- 登记一个已有二进制路径，systemd restart 后健康检查通过。
- 上传一个测试 HTTP 二进制，部署后健康检查通过。
- 修改版本后重新部署，旧版本仍在 release 列表。
- 选择旧版本回滚，运行态 active_release_id 更新。

### 阶段 3：多节点部署

目标：支持把应用部署到多个目标节点，并从总览看到分布状态。

实现单元：

1. SSH 节点执行器
   - 已支持远程执行命令、上传文件、创建目录。
   - 已支持节点探测和能力缓存。

2. 应用目标计划
   - 已支持一个应用关联多个节点。
   - 已支持部署任务按节点拆分执行并聚合结果。

3. 分布式状态视图
   - 应用状态已显示每个目标节点的部署版本、健康状态、最后任务。
   - 服务页已按目标节点展示同一服务的分布状态。

验收场景：

- 同一 compose 应用部署到两个 SSH 节点。
- 一个节点失败时，任务显示部分失败，不覆盖其他节点成功状态。
- 节点离线时，预检阶段阻塞该节点子任务。

### 阶段 4：单机低停机更新

目标：二进制应用支持 blue/green，尽量不停机切换。

实现单元：

1. blue/green systemd unit
   - 已维护 active/standby slot。
   - 已支持 base_port 和 standby_port。

2. Caddy/Nginx 切流
   - 已支持新槽位健康检查通过后切换 Caddy/Nginx 反代。
   - 待补切换失败时自动停止新槽位并明确保持旧槽位的运行提示。

3. 回滚
   - 已支持选择旧 release 重新部署。
   - 待补快速切回上一槽位的专用操作入口。

验收场景：

- 新版本健康检查失败时，旧版本继续服务。
- 新版本健康检查成功后，反代切到新端口。
- 回滚能切回上一 release。

## 当前 UI 对应的数据来源

总览页：

- 应用数：apps
- 服务数：解析 compose services + systemd service
- 节点数：nodes
- 运行任务数：operation_tasks where status in running states
- 应用表格：apps + app_runtime_states + latest deployment_runs
- 节点列表：nodes + latest node_checks
- 部署任务：operation_tasks

应用页：

- 主对象：apps
- 详情：runtimefs + app_runtime_states
- 历史：deployment_runs + app_config_snapshots

服务页：

- 派生索引：compose services / systemd service
- 日志：deploy log reader

节点页：

- nodes
- node_capabilities
- node_checks

模板页：

- catalog registry，不依赖数据库。

制品页：

- artifacts
- app_releases

## 暂不纳入第一阶段

- 完整应用市场
- 实时日志 websocket
- PostgreSQL/Redis 数据备份和数据恢复
- Kubernetes
- 跨节点自动调度和负载均衡
- 高级审批流

这些能力并非不重要，但会明显增加理解成本和部署复杂度，不符合第一阶段“简单容易上手”的目标。

## 近期建议实施顺序

1. 继续收敛用户可见页面里的占位文案，把已经实现的 Compose、二进制、多节点、任务、制品和设置能力都指向真实入口。
2. 补“回滚”专用语义：当前旧 release 可激活并重启，但页面需要更明确地区分“激活旧版本”和“回滚到上一稳定版本”。
3. 补部署差异预览：部署确认页已经列出文件计划，下一步应展示 compose/env/systemd/release/current 的关键内容差异。
4. 补节点安装任务：目前节点详情给出 Docker/Compose/systemd/Caddy/Nginx 安装建议，后续可把高频安装动作接入任务系统。
5. 补服务级健康检查明细和最近一次检查结果，让服务页不只显示聚合状态。

这样可以在已可用的 Compose、二进制和多节点基础上，继续把回滚、差异预览、节点安装和健康明细做得更清晰易用。
