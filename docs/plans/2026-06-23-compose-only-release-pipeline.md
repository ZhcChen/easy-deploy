---
date: 2026-06-23
topic: compose-only-release-pipeline
status: active
origin: docs/brainstorms/2026-06-23-compose-only-deployment-requirements.md
---

# Compose Only 发布流水线重构方案

## 问题框架

`easy-deploy` 当前同时承载两条应用部署主线：

- Compose 应用部署
- Binary + systemd 应用部署

这导致产品模型、数据模型、任务日志、OpenAPI、页面结构都出现双轨负担：

- 应用创建和详情页同时暴露 Compose 与 Binary 字段
- 任务系统同时承载 `compose.*` 与 `binary.*`
- 发布版本页只服务二进制版本，和 Compose 主线割裂
- OpenAPI 既承担配置控制，又承担版本上传，外部接入心智负担过高

这与项目“简单易用，但功能不能缺失”的定位冲突。

本轮重构要把产品重新收敛到单一主线：

- 应用运行统一使用 `docker compose`
- 版本包仍由外部项目通过 OpenAPI 推送
- 平台内部负责配置、排队、串行发布、日志与回滚入口
- 删除应用级 Binary + systemd 链路，不再把它作为产品能力继续演进

## 需求继承

以下约束直接继承自 `docs/brainstorms/2026-06-23-compose-only-deployment-requirements.md`：

1. 所有业务部署统一走 Compose
2. 移除应用级 Binary + systemd 能力链
3. OpenAPI 只保留“推送版本包”能力
4. 应用创建、节点绑定、Compose 配置、部署脚本配置全部在后台页面完成
5. 新版本一旦被平台接收，自动登记并自动进入发布队列
6. 同一应用的多个版本必须严格串行发布，不能折叠为“只保留最新”
7. 发布执行模型改为“脚本驱动的多阶段流水线”
8. 日志要支持任务级、阶段级、步骤级、原始输出级，且默认折叠详细输出
9. 蓝绿部署保留，但必须收敛到 Compose 层，不能再依赖 Binary slot + systemd

## 范围边界

### 本轮包含

- 应用模型收敛为 Compose 主线
- 版本包模型从 Binary artifact 重定义为通用 release package
- 新的发布队列与串行调度模型
- 新的阶段/步骤/原始输出日志模型
- 应用详情页与发布版本页的结构重做
- OpenAPI 收缩为单一版本投递接口
- 应用级 Binary 页面、路由、字段、任务类型的退场方案

### 明确不做

- Git tag 自动拉取和自动构建
- 平台主动轮询外部版本源
- Kubernetes 或容器编排集群调度
- 平台级 Redis、MQ、外部任务队列依赖
- 回滚外部数据目录或数据库内容
- 代理层的通用自动生成模板体系

## 目标状态总览

### 产品模型

平台只保留一个“应用”主对象，但应用内部区分两种发布来源：

- `manual`：不依赖版本包，适合 Redis、PostgreSQL、Caddy、Nginx 等内置或手工维护的 Compose 应用
- `package_upload`：依赖外部版本包投递，适合业务应用

这不是新的“应用类型”分叉，而是同一 Compose 应用的发布来源配置。

同时保留两种 Compose 发布策略：

- `recreate`：直接更新当前运行集合
- `blue_green`：基于 Compose slot 的蓝绿切换

### 最终对象模型

为了把“简单易用”和“功能不缺失”同时成立，最终用户只需要理解 5 个一等对象：

1. `节点`
   - 部署目标机器
   - 负责 SSH、目录、Docker/Compose 能力和执行环境

2. `应用`
   - 平台中的唯一业务主体
   - 负责目标节点、Compose 配置、环境变量、部署脚本、发布策略、健康检查

3. `发布版本`
   - 某个应用接收到的一个可发布版本包
   - 负责版本号、发布时间、来源、校验和、包路径、解压目录和发布状态

4. `发布队列`
   - 某个应用下待执行或已执行的版本串行队列
   - 负责顺序、触发来源、当前状态、关联任务和执行摘要

5. `部署任务`
   - 平台真正执行一次发布时生成的运行记录
   - 负责阶段、步骤、日志、节点执行结果和最终结论

以下对象应降级为“派生视图”而不是产品主对象：

- `服务`
  - 只是应用下 Compose services 的解析结果
  - 保留查看与日志入口，但不再作为一级创建或一级操作对象

- `模板`
  - 只是创建应用时的初始化方式
  - 不再承担长期运行态管理职责

- `制品`
  - 统一收敛为“发布版本”
  - 不再保留一个与应用和任务平行的独立产品心智

### 用户主路径

最终产品主路径应稳定为：

1. 创建应用
2. 绑定节点
3. 维护 Compose 配置与部署脚本
4. 接收版本包
5. 自动或手动进入发布队列
6. 串行发布
7. 查看任务阶段日志
8. 必要时回滚到旧版本 + 旧配置快照

如果一个页面、一个 API 或一个字段不能服务这条主路径，就应优先考虑降级、隐藏或删除。

### 交付边界

- 后台页面负责配置应用、节点、环境变量、脚本、健康检查
- 版本包通过后台上传或 OpenAPI 上传进入同一发布版本中心
- 平台接收版本包后自动入队
- 平台内部工作线程按应用维度串行发布
- 任务与日志全部在后台页面查看
- OpenAPI 不再提供创建应用、更新配置、触发部署、读取节点、读取任务等控制面能力

### 菜单收敛

为了进一步降低认知负担，一级导航建议稳定为：

- `总览`
- `应用`
- `发布版本`
- `节点`
- `凭据`
- `部署任务`
- `权限`
- `系统`

其中：

- `服务`
  - 降级为应用详情中的只读运行视图，或作为二级页保留日志定位入口
  - 不再作为与“应用”并列的产品对象进行强化

- `模板`
  - 保留入口，但定位为“新建应用辅助器”
  - 不再作为高频长期操作页面

- `API Token`
  - 继续挂在权限/系统域下，不单独强化为更高层级

## 核心技术决策

### 1. 不再把 `app_type=binary` 作为可用产品分支

当前 `apps.app_type`、`deploy_mode`、`app_binary_configs`、`binary_artifacts`、Binary 路由和模板，构成了一整套独立产品线。继续维护会持续放大复杂度。

本轮方案中：

- UI 和业务逻辑只暴露 Compose 应用
- 旧 Binary 数据保留为历史兼容数据，不再继续写入
- 应用级 systemd unit 生成、二进制路径、Binary stop/restart、Binary Blue/Green 页面全部退出主流程

平台自身的 systemd 部署方式不在删除范围内。这里删除的是“应用部署用 systemd”，不是“easy-deploy 自身的服务部署方式”。

### 2. 版本包被定义为“Opaque Release Package”

版本包继续沿用用户已确认的命名约束，由外部项目推送：

```text
{service_key}_version_{x_y_z}.tar.gz
```

平台只负责：

- 校验服务标识和版本号
- 保存原始包
- 解压到 release 目录
- 记录校验和、发布时间、来源和发布状态

平台不强制规定包内必须包含固定业务文件结构。部署脚本通过约定好的环境变量读取解压目录，自行决定如何消费版本内容。

这样可以同时支持：

- 只上传镜像引用锁文件
- 上传静态资源产物
- 上传额外模板或配置片段
- 上传由业务项目自己定义的部署辅助文件

### 3. 发布来源和发布策略分离

为避免再次出现语义混乱，应用配置要拆成三个独立概念：

- `release_source`：`manual` / `package_upload`
- `compose_strategy`：`recreate` / `blue_green`
- `deploy_strategy`：继续沿用现有语义，表示节点滚动策略，如 `rolling_stop_on_failure` / `rolling_continue`

不在本轮强行重命名历史列 `apps.deploy_strategy`。数据库层先新增 `compose_strategy` 和 `release_source`，避免和现有节点滚动策略混义。

### 4. 单应用串行发布采用显式队列表，不复用“单 active task”做隐式排队

当前 `operation_tasks` 上的 guard 只适合阻止同一应用同时存在一个 active deploy task，不适合表达“多个版本排队等待发布”。

本轮新增显式队列表，队列顺序以“平台接收版本包的顺序”为准，而不是以 `version_code` 为准。原因：

- `version_code` 适合列表排序，不适合表达真实入队先后
- 迟到的老版本包也应按接收顺序排队，避免隐式插队
- 这样更符合“连续推送多个版本，平台逐个串行发布”的产品语义

### 5. 阶段日志增加独立表，而不是把阶段继续塞进 `operation_task_steps`

当前已有 `operation_task_steps`，但它只能稳定表示“步骤”，不足以表达：

- 任务级
- 阶段级
- 步骤级
- 原始输出级

因此新增 `operation_task_phases` 作为中间层，模型收敛为：

- `operation_tasks`：整个发布任务
- `operation_task_phases`：发布阶段
- `operation_task_steps`：阶段内的命令或脚本步骤
- `operation_task_logs`：挂在 step 上的原始输出

### 6. 蓝绿部署只保留 Compose 级语义，不再内建代理模板生成

本轮保留蓝绿能力，但收敛为：

- 平台负责 active / standby slot 选择
- 平台负责渲染 slot 对应的运行目录和变量
- 平台负责记录切流阶段和日志
- 具体如何切流，由应用配置中的 `switch_traffic` 脚本负责

这样平台不需要内建面向 Caddy / Nginx 的复杂模板体系，也不会重新回到 Binary Blue/Green 的重实现路径。

### 7. 参考 qfy-sc 的模板形态，但不耦合 qfy-sc 业务逻辑

`qfy-sc` 的测试环境与生产环境模板已经体现了后续 easy-deploy 应该支持的通用契约：

- 一个部署单元就是一个 Compose 应用目录
- `app.yaml.example` 描述应用标识、环境、健康检查和备注
- `compose.yaml.example` 描述真实运行服务、网络、端口、日志 label 和 healthcheck
- `.env` 保存环境差异与敏感配置占位
- `scripts/` 保存迁移、修复、seed、健康检查等有序发布动作

平台需要吸收的是这套“模板契约”，不是吸收 qfy-sc 的业务规则。也就是说：

- qfy-sc 的 `00-migrate-worker.sh`、`30-seed-core.sh`、`90-healthcheck.sh` 这类脚本名只能作为用户配置，不进入平台内置枚举
- qfy-sc 的测试域名、端口、数据库拓扑、日志链路只能存在于应用配置和脚本里，不进入平台代码分支
- 测试环境和正式环境的差异通过应用配置、环境变量、目标节点、脚本开关和配置快照表达，不通过平台内置“qfy-sc 测试/正式”逻辑表达

由此得到的产品能力是“模板导入 + 可视化编辑 + 发布计划”，而不是“内置 qfy-sc 部署向导”。

### 8. 发布版本事实与发布时间计划分离

版本包上传只表示平台“收到一个可发布版本”，不等于必须立即执行发布。

发布控制应拆成三种模式：

- 自动上传即入队：上传后立即创建队列项，调度器按应用串行执行
- 手动发布：上传后只登记为发布版本，用户在后台选择版本后创建队列项
- 定时发布：上传后登记版本，用户选择发布时间，调度器只在到达时间后把它推进执行

因此 `app_releases` 只负责版本事实，`app_release_queue` 或后续发布计划结构负责执行计划。`scheduled_publish_at` 如果短期先放在 `app_releases`，也只能作为过渡字段；最终语义应归到队列/计划层，避免同一个版本事实被多个发布时间污染。

### 9. 从 qfy-sc 测试环境提炼出的通用部署契约

`qfy-sc` 测试环境模板进一步确认了 easy-deploy 应该管理的是“发布单元”，而不是某个业务项目的内部语义。一个复杂业务系统会拆成多组 Compose 应用，例如基础设施、Worker、Backend、Gateway、前端静态站点，每组都可以独立发布、独立查看日志、独立健康检查。

对 easy-deploy 来说，应抽象成以下通用规则：

- 基础设施类应用走 `manual` 发布来源，不要求版本包；例如 PostgreSQL、Redis、NATS、Loki、Alloy、Gateway。
- 业务运行类应用走 `package_upload` 发布来源，由业务项目或 CI 通过 OpenAPI 推送版本包；例如 backend、worker、admin、merchant-admin、supplier-admin、oc-web。
- 首次部署顺序、migration、seed、mq repair、healthcheck 这类流程全部由用户在应用脚本槽位中配置，不进入平台内置枚举。
- 测试环境和正式环境使用同一套平台能力，差异通过目标节点、域名、Compose、`.env`、密钥、脚本开关和配置快照表达。
- 业务仓库只保留可提交的模板和构建产物，不维护远程部署逻辑；服务器发布、制品分发、服务启停、网关切换、回滚和执行日志由 easy-deploy 承接。

这意味着后续“模板导入”能力应支持从目录中读取 `app.yaml.example`、`compose.yaml.example`、`.env.example` 和 `scripts/`，但导入后仍然生成普通应用配置，不生成 qfy-sc 专属分支。

## 数据模型收敛方案

### 保留并继续复用

- `apps`
- `app_targets`
- `app_runtime_states`
- `app_config_snapshots`
- `operation_tasks`
- `operation_task_logs`
- `operation_task_steps`
- `deployment_runs`

### 停止继续写入的旧表

- `binary_artifacts`
- `app_binary_configs`

这些表短期保留历史数据，不在本轮直接删除，避免高风险迁移。页面和业务逻辑不再依赖它们。

### 数据分层原则

为了避免“一个表同时承载配置、事实、队列和日志”再次失控，后续数据模型按四层收敛：

1. 配置层
   - `apps`
   - `app_targets`
   - `app_config_snapshots`
   - 表达用户配置的期望状态，不记录一次次执行尝试

2. 版本层
   - `app_releases`
   - 表达平台收到过哪些可发布版本包，是版本事实表，不承载调度顺序

3. 编排层
   - `app_release_queue`
   - 表达哪些 release 已入队、按什么顺序执行、当前卡在什么位置

4. 执行层
   - `operation_tasks`
   - `operation_task_phases`
   - `operation_task_steps`
   - `operation_task_logs`
   - `deployment_runs`
   - 表达一次发布实际做了什么、在哪个阶段失败、输出了什么日志

分层约束：

- 配置层只描述“应该怎样部署”
- 版本层只描述“收到过什么版本”
- 编排层只描述“准备怎么排队执行”
- 执行层只描述“这次实际执行了什么”
- 除 `app_runtime_states` 这种投影视图外，不允许把执行期事实反向写回配置层，避免“当前状态”和“用户配置”互相污染

### 新增或扩展的结构

#### `apps`

新增字段：

- `release_source`：`manual` / `package_upload`
- `compose_strategy`：`recreate` / `blue_green`

继续保留现有：

- `deploy_strategy`：节点滚动策略
- `work_dir`
- `environment`

#### `app_releases`

新增通用发布版本表，替代 `binary_artifacts` 的产品职责。

建议字段：

- `id`
- `app_id`
- `version`
- `version_code`
- `package_name`
- `package_path`
- `extract_dir`
- `status`：`received` / `queued` / `deploying` / `deployed` / `failed` / `rolled_back`
- `source`：`openapi` / `web`
- `checksum_sha256`
- `size_bytes`
- `published_at`
- `received_at`
- `metadata`

列表排序规则：

- 版本中心默认按 `version_code DESC, published_at DESC, id DESC`

队列执行规则：

- 不按 `version_code` 排队
- 按 `queue_seq ASC` 排队

#### `app_release_queue`

新增显式队列表，用于表达同一应用多个版本的串行发布顺序。

建议字段：

- `id`
- `app_id`
- `release_id`
- `config_snapshot_id`
- `queue_seq`
- `status`：`queued` / `running` / `success` / `failed` / `canceled`
- `triggered_by`
- `message`
- `task_id`
- `scheduled_publish_at`
- `created_at`
- `started_at`
- `finished_at`

关键约束：

- 同一应用同一时刻只允许一个 `running`
- 同一 release 只允许一个活跃队列项
- `queue_seq` 单调递增，不回填
- 调度器只拉取 `scheduled_publish_at` 为空或已到期的 `queued` 项

#### `operation_tasks`

建议新增：

- `release_id`

这样任务详情、应用详情和版本中心能直接关联到对应 release。

#### `operation_task_phases`

新增阶段表。

建议字段：

- `id`
- `task_id`
- `phase_no`
- `phase_key`
- `title`
- `status`
- `summary`
- `started_at`
- `finished_at`
- `created_at`
- `updated_at`

#### `operation_task_steps`

建议新增：

- `phase_id`

并让 `step_no` 语义调整为“阶段内顺序”或保留“任务内顺序 + phase_id”。

#### `deployment_runs`

建议新增：

- `release_id`

`deploy_action` 的旧值短期兼容保留，但新的写入语义收敛为：

- `release_deploy`
- `release_rollback`
- `manual_compose_apply`

这样不会再把新模型写成 `binary_restart` / `compose_up` 这类历史语义。

### 运行态索引的职责边界

`app_runtime_states` 继续保留，但角色必须降级为“当前摘要索引”，而不是历史事实表。

它只应该承载：

- 当前生效 release 摘要
- 当前 active slot
- 最近一次健康检查结果
- 最近一次任务状态摘要
- 页面列表所需的轻量运行态字段

它不应该承载：

- 发布历史
- 队列历史
- 阶段日志
- 版本包事实
- 可追责的操作流水

这样做的意义是：

- 列表页仍然可以快速读取“当前状态”
- 真实历史全部回到 `app_releases`、`app_release_queue`、`operation_tasks*`
- 后续即使需要重建运行态，也可以从“最近成功发布 + 配置快照”重新投影，而不是依赖一个不断膨胀的宽表

### 配置快照策略

`app_config_snapshots` 继续作为“应用配置历史”的主表使用。

本轮不新增大量显式脚本列，采用以下策略：

- `compose_content` 继续存 Compose 原文
- `env_content` 继续存环境变量原文
- 脚本配置、健康检查、`release_source`、`compose_strategy`、slot 配置等进入 `metadata`

关键规则：

- 每次应用配置保存，立即生成新的 `config_snapshot`
- 对于 `package_upload` 应用，版本包入队时绑定当前 `config_snapshot_id`
- 后续即使用户修改了配置，已入队版本仍按当时绑定的快照部署

这保证：

- 每次发布都能追溯“哪个版本 + 哪份配置”
- 队列中的旧版本不会被后续配置编辑悄悄污染

## 后端模块收敛方案

当前主要问题不是单点 bug，而是产品模型和代码模型还没有收敛到同一条主线。

截至当前工作区，几个热点文件已经明显超过舒适维护规模：

- `api/src/web/mod.rs`：约 13123 行
- `api/src/apps.rs`：约 10556 行
- `api/src/auth/service.rs`：约 2664 行
- `api/src/nodes.rs`：约 2515 行
- `api/src/deploy.rs`：约 2041 行

其中真正的高风险点是前两个：

- `api/src/apps.rs` 同时承载应用配置、Binary 兼容、版本包、队列、调度、运行态和部分部署逻辑
- `api/src/web/mod.rs` 同时承载后台页面、表单提交、OpenAPI、在线文档、Binary 页面和任务接口

这会直接带来两个后果：

1. 产品上已经决定 Compose-only，但代码里仍然到处存在 `app_type == "binary"` 分支
2. 页面路由与对外 OpenAPI 混杂，导致任意一个页面调整都可能顺手污染公开接口语义

### `apps` 域拆分

建议把 `api/src/apps.rs` 收敛为目录模块，主目标不是“为了好看”，而是把配置、版本、队列、运行态和遗留兼容拆成稳定边界：

- `api/src/apps/mod.rs`
  - 对外导出统一服务入口与共享类型
- `api/src/apps/domain.rs`
  - 应用、版本、队列、快照等核心领域模型
- `api/src/apps/service.rs`
  - 应用列表、详情、保存配置、目标节点绑定
- `api/src/apps/config.rs`
  - Compose 配置、环境变量、脚本、健康检查、快照生成
- `api/src/apps/releases.rs`
  - 版本包命名校验、上传落盘、解压、版本记录
- `api/src/apps/queue.rs`
  - 入队、出队、串行发布锁、失败阻塞、重试/取消
- `api/src/apps/runtime.rs`
  - `app_runtime_states` 投影、运行态汇总、当前 release 摘要
- `api/src/apps/legacy_binary.rs`
  - 仅作为阶段性隔离层存在，用于托管 Binary 历史兼容逻辑，阶段 E 后整体删除

收敛原则：

- `apps/*` 负责“应用配置和发布事实”
- `deploy/*` 负责“如何执行一次发布”
- `legacy_binary.rs` 只能被过渡路径引用，不能再向 Compose 主链路泄漏字段和判断

### `deploy` 域拆分

`api/src/deploy.rs` 不应继续承载配置解释、页面语义和应用选择逻辑。它只保留执行编排职责，建议最终收敛为：

- `api/src/deploy/mod.rs`
- `api/src/deploy/pipeline.rs`
  - 发布阶段编排与阶段状态写入
- `api/src/deploy/executor.rs`
  - 本地或远端命令执行包装
- `api/src/deploy/compose.rs`
  - Compose up/down/recreate/blue-green 切换
- `api/src/deploy/health.rs`
  - 健康检查与等待逻辑
- `api/src/deploy/logging.rs`
  - 阶段、步骤、原始输出归档

边界要求：

- `deploy/*` 不直接决定某个应用是否应该入队
- `deploy/*` 不直接解释 OpenAPI 入参
- `deploy/*` 接收已经确定好的 `release + config_snapshot + targets`，只负责把它执行出来

### `web` 域拆分

`api/src/web/mod.rs` 当前仍同时暴露以下旧链路：

- `/apps/{app_id}/binary/*`
- `/api/v1/apps*`
- `/api/v1/nodes`
- `/api/v1/tasks*`
- 应用级 Binary 在线文档与示例

这说明当前“页面域”和“对外接口域”仍纠缠在一起。建议按页面/接口域拆成以下模块：

- `api/src/web/mod.rs`
  - 只保留路由装配、共享中间件和通用 helpers
- `api/src/web/dashboard.rs`
  - 总览
- `api/src/web/apps.rs`
  - 应用列表、详情、配置保存、部署入口
- `api/src/web/releases.rs`
  - 发布版本中心、队列视图、版本操作
- `api/src/web/tasks.rs`
  - 部署任务与阶段日志
- `api/src/web/nodes.rs`
  - 节点、探测、事件日志
- `api/src/web/credentials.rs`
  - 凭据、公钥、查看与复制
- `api/src/web/rbac.rs`
  - 账号、角色、权限、API Token
- `api/src/web/settings.rs`
  - 系统设置、自动入队/定时发布等策略
- `api/src/web/openapi.rs`
  - 仅保留版本包投递接口与公开文档

拆分原则：

- 后台页面路由和公开 OpenAPI 不能再放在一个超大文件里共生演化
- “发布版本”页要与历史 `artifacts` 语义切开，避免产品语言继续漂移
- OpenAPI 文档示例必须只围绕“投递版本包”一条主链路

## 退场顺序与收敛优先级

当前最重要的不是继续扩能力，而是停止继续在双模型上加功能。建议按以下优先级推进：

### 第一优先：先完成产品模型和菜单收敛

对应阶段重点：

- 先完成阶段 C 的页面收口
- 让左侧菜单、应用详情、发布版本页、任务页都只讲 Compose-only 主链路
- 把 `服务` 降级为应用详情视图，把 `模板` 降级为创建辅助器

原因：

- 如果产品对象不先收敛，后端每拆一次模块都会继续被旧概念拖回去

### 第二优先：再拆 `apps.rs` 与 `web/mod.rs`

对应阶段重点：

- 在不改产品语义的前提下先做模块切分
- 让新功能只能落在新边界里，旧文件只做过渡转发

原因：

- 当前真正拖慢后续开发速度的，不是缺某个按钮，而是两个超大文件让每次修改都扩大影响面

### 第三优先：最后删除应用级 Binary / systemd 主链路

对应阶段重点：

- 阶段 A / B 建立新 release + queue + task phase 模型
- 阶段 C / D 完成页面和 OpenAPI 收缩
- 阶段 E 再正式删除 Binary 路由、字段写入、模板和文档

原因：

- 先建新主链路，再删旧主链路，迁移风险最低
- 但在阶段 E 之前，Binary 逻辑必须被隔离到 `legacy_binary`，不能继续扩散

### 补充约束

- easy-deploy 自身通过 systemd 部署保留，不在删除范围内
- 删除范围仅限“业务应用通过 Binary + systemd 部署”的产品能力
- 在 Binary 主链路退场前，不再接受任何新增 Binary 相关需求和 UI 优化

## RuntimeFS 设计

### 目标目录结构

```text
<data_dir>/apps/<app_key>/
├── compose.yaml
├── .env
├── .easy-deploy/
│   ├── app.yaml
│   └── scripts/
│       ├── pre_deploy.sh
│       ├── deploy.sh
│       ├── post_deploy.sh
│       ├── switch_traffic.sh
│       └── cleanup.sh
├── releases/
│   └── <version>/
│       ├── package.tar.gz
│       ├── bundle/
│       ├── render/
│       │   ├── compose.yaml
│       │   ├── .env
│       │   └── runtime.env
│       └── release.yaml
├── current
└── slots/
    ├── blue
    └── green
```

说明：

- `compose.yaml` 和 `.env` 代表应用当前基线配置
- `.easy-deploy/scripts/` 保存用户在后台配置的阶段脚本
- 每个 release 都有独立目录，保存原始包、解压内容和渲染结果
- `current` 指向当前生效 release
- `slots/blue` / `slots/green` 只对 `blue_green` 应用使用

### 将被删除的 RuntimeFS 能力

`api/src/runtimefs.rs` 中以下应用级 Binary 文件生成逻辑要退出：

- `save_binary_runtime_files`
- `save_binary_release_file`
- `load_binary_runtime_files`
- `BinaryRuntimeMetadata`
- `BinaryRuntimeConfig`
- `systemd/` 目录和相关 unit/env 渲染逻辑

保留并继续增强的部分：

- `compose.yaml`
- `.env`
- `.easy-deploy/app.yaml`
- `releases/`
- `current`
- `DEPLOY_SCRIPT_FILE_NAME`

## 发布流水线设计

### 任务类型

发布相关任务统一收敛为：

- `release.receive`
- `release.deploy`
- `release.rollback`
- `release.manual_apply`

其中：

- `release.receive` 可只作为事件记录，不一定要生成长任务
- `release.deploy` 是主流程
- `release.manual_apply` 用于 `manual` 应用直接部署当前配置

### `release.deploy` 标准阶段

默认阶段顺序：

1. `prepare`
2. `render`
3. `preflight`
4. `pre_deploy`
5. `deploy`
6. `post_deploy`
7. `health_check`
8. `finalize`

`blue_green` 应用额外插入：

9. `switch_traffic`
10. `cleanup`

### 每层日志含义

- 任务级：本次发布整体状态
- 阶段级：如 `preflight`、`deploy`、`health_check`
- 步骤级：某条命令、某个脚本、某个节点执行单元
- 原始输出级：stdout / stderr / combined

默认 UI 行为：

- 默认展开任务摘要
- 阶段默认展开标题、状态和摘要
- 步骤默认折叠
- 原始输出默认折叠

### 脚本执行模型

为了降低复杂度，本轮不做“无限自定义步骤编排器”，而是提供固定阶段槽位：

- `pre_deploy`
- `deploy`
- `post_deploy`
- `switch_traffic`
- `cleanup`

每个槽位可为空。`deploy` 为必填。

脚本执行时平台注入统一环境变量：

- `ED_APP_ID`
- `ED_APP_KEY`
- `ED_APP_NAME`
- `ED_ENVIRONMENT`
- `ED_APP_DIR`
- `ED_RELEASE_ID`
- `ED_RELEASE_VERSION`
- `ED_RELEASE_DIR`
- `ED_RELEASE_BUNDLE_DIR`
- `ED_RELEASE_RENDER_DIR`
- `ED_CURRENT_LINK`
- `ED_TARGET_NODE_KEY`
- `ED_TARGET_NODE_NAME`
- `ED_COMPOSE_STRATEGY`
- `ED_ACTIVE_SLOT`
- `ED_STANDBY_SLOT`

这样平台负责编排上下文，业务脚本只负责消费 release 目录与运行环境。

## 蓝绿部署方案

### 设计原则

蓝绿部署继续保留，但要足够克制：

- 不再生成应用级 systemd slot unit
- 不再内建 Caddy / Nginx 专用模板
- 不再把切流策略做成平台内置分支爆炸

### 平台职责

- 记录当前 active slot
- 决定 standby slot
- 为目标 slot 生成渲染目录
- 在任务阶段中显式记录 `switch_traffic`
- 发布成功后更新 slot 指针

### 应用脚本职责

- 如何用 Compose 项目名区分 `blue` / `green`
- 如何让流量进入新 slot
- 如何处理旧 slot 回收

推荐约定：

- `ED_ACTIVE_SLOT=blue|green`
- `ED_STANDBY_SLOT=green|blue`
- `ED_RELEASE_RENDER_DIR` 指向当前发布版本渲染目录

这样蓝绿能力仍由平台统一建模，但具体技术细节交给应用的部署脚本处理。

## 串行发布队列设计

### 队列规则

对于 `release_source=package_upload` 的应用：

- 接收到版本包后，立即登记 `app_releases`
- 同时创建 `app_release_queue` 项
- 如果当前无运行中的同应用发布任务，则调度器立刻拉起下一条 `queued`
- 如果已有运行中任务，则新版本保持 `queued`

### 顺序保证

必须保证：

- 同一应用严格串行
- 连续多个版本都要逐个执行
- 不做“保留最新、丢弃中间版本”的折叠优化

### 失败语义

默认策略：

- 某个版本发布失败，不自动跳过并继续下一个版本
- 当前应用队列停止在失败版本
- 运维在后台选择“重试当前版本”或“取消当前版本并继续后续版本”

原因：

- 这更符合“可追踪、可介入”的运维预期
- 自动跨过失败版本会让环境状态更难解释

## 页面结构方案

### 左侧导航收敛

建议保留：

- 总览
- 应用
- 节点
- 凭据
- 任务
- 发布版本
- API Token
- 在线文档
- 设置

不再保留与 Binary 主线强耦合的导航心智。

### 应用列表页

应用列表继续作为主入口，但只展示 Compose 应用。

列表重点字段建议保留：

- 应用名
- 环境
- 发布来源
- 发布策略
- 当前版本
- 当前状态
- 最近更新时间
- 操作

不再显示：

- 应用类型中的 `binary`
- 部署目录大段路径
- 旧 Binary 专属状态提示

### 应用详情页

应用详情页建议改成三个主分区：

1. `部署配置`
2. `发布队列`
3. `执行记录`

#### 部署配置

包含：

- 基础信息
- 目标节点
- 发布来源
- 发布策略
- Compose 配置
- 环境变量
- 部署脚本
- 健康检查

#### 发布队列

包含：

- 当前生效版本
- 正在发布中的版本
- 待发布版本列表
- 最近失败版本
- 手动部署当前配置入口（仅 `manual` 应用）

#### 执行记录

按“任务 > 阶段 > 步骤 > 原始输出”折叠展示。

### 发布版本页

发布版本页成为统一版本中心，职责包括：

- 手工上传版本包
- 展示最近版本
- 按应用过滤
- 查看队列状态
- 查看发布时间、接收时间、来源、校验和
- 发起重试 / 回滚 / 取消队列项

不再把它定义成“二进制版本页”。

## OpenAPI 收缩方案

### 对外保留

只保留一个对外控制面能力：

- `POST /api/v1/services/{service_key}/packages`

该接口职责：

- 接收版本包
- 校验文件名和应用归属
- 写入版本记录
- 自动入队
- 返回 release 和 queue 的基础信息

### 对外移除

以下接口不再作为 OpenAPI 能力保留：

- 创建应用
- 更新应用配置
- 触发部署
- 启用 / 停用应用
- 读取节点
- 读取任务
- 读取应用详情

后台页面仍然可以保留自身页面路由和服务端逻辑，但这些不再作为公开 OpenAPI 文档的一部分。

### 文档要求

公开在线文档只需要讲清楚四件事：

1. 版本包命名规则
2. 请求字段与示例
3. 平台自动入队和自动发布的语义
4. 常见错误码与错误提示

## 迁移实施顺序

### 阶段 A：引入新 release 与 phase 模型

目标：

- 不动现有 UI 主流程，先把新表和新任务层补齐

实施单元：

- 新增 `app_releases`
- 新增 `app_release_queue`
- 新增 `operation_task_phases`
- 给 `operation_task_steps` 增加 `phase_id`
- 给 `operation_tasks` / `deployment_runs` 增加 `release_id`

验证重点：

- 迁移可重复执行
- 历史数据不丢失
- 旧功能仍能启动

### 阶段 B：接入 package upload -> queue -> deploy 主链路

目标：

- 新版本包上传后自动入队，调度器能按应用维度拉起发布

实施单元：

- 新建 release 服务层
- 上传包保存与安全解压
- 调度器与串行发布锁
- 新的阶段/步骤日志写入

验证重点：

- 同一应用多版本连续推送能严格串行
- 失败版本能阻塞后续，等待人工处理

### 阶段 C：页面切到 Compose-only 视图

目标：

- 后台页面不再出现 Binary 主线

实施单元：

- 重做应用创建页
- 重做应用详情页
- 重做发布版本页
- 重做任务详情页的阶段日志视图

验证重点：

- 页面无 Binary 字段残留
- 发布队列和任务日志能从 UI 直接读懂

### 阶段 D：OpenAPI 收缩

目标：

- 外部接入面只剩版本投递接口

实施单元：

- 删除或下线公开文档中的旧接口
- 收缩 `openapi.json`
- 更新在线文档示例和错误码

验证重点：

- 文档与实际接口行为一致
- 无鉴权公开文档只暴露版本投递说明

### 阶段 E：删除应用级 Binary 旧链路

目标：

- 清理应用级 Binary / systemd 逻辑与模板

实施单元：

- 删除 `api/src/apps.rs` 中 Binary task 分支
- 删除 `api/src/runtimefs.rs` 中 Binary runtime 渲染
- 删除 `api/src/web/mod.rs` 中 Binary 页面与 Binary OpenAPI
- 删除相关模板段落与测试
- 更新 `api/src/maintenance.rs`，不再以 Binary 表作为活跃业务表

验证重点：

- 工作区不存在可访问的 Binary 应用入口
- Compose-only 新主链路完整可用

## 实施单元

### U1：Schema 与任务层重构

目标：

- 为 release 队列、阶段日志和新发布语义建立稳定数据层

主要文件：

- `api/migrations/0038_compose_release_queue.sql`
- `api/migrations/0039_task_phases.sql`
- `api/src/tasks.rs`
- `api/src/apps.rs`

测试文件：

- `api/src/tasks.rs`
- `api/src/apps.rs`

测试场景：

- 同一应用可存在多个 `queued` release，但同时只能有一个 `running`
- 队列顺序按接收顺序，不按 `version_code`
- 阶段、步骤、日志能正确挂接
- 旧 `deployment_runs` 历史记录仍可读取

### U2：RuntimeFS 与版本包落盘重构

目标：

- 从 Binary runtime 文件系统切到 Compose-only release 目录模型

主要文件：

- `api/src/runtimefs.rs`
- `api/src/apps.rs`

测试文件：

- `api/src/runtimefs.rs`
- `api/src/apps.rs`

测试场景：

- 非法包名、非法解压路径被拒绝
- 版本包可安全保存并解压到独立 release 目录
- `systemd/` 和 unit/env 渲染不再出现
- `current` 和 `slots/` 指针更新正确

### U3：发布调度器与脚本流水线

目标：

- 让 release 能从入队走到多阶段发布与健康检查

主要文件：

- `api/src/apps.rs`
- `api/src/tasks.rs`
- `api/src/deploy/mod.rs`

测试文件：

- `api/src/apps.rs`
- `api/src/tasks.rs`

测试场景：

- `package_upload` 应用上传后自动创建队列项
- `manual` 应用不会被要求先上传版本包
- 发布失败会阻塞后续版本
- `blue_green` 应用能得到正确的 slot 环境变量

### U4：后台 UI Compose-only 收敛

目标：

- 页面只讲 Compose 应用、发布队列和执行日志

主要文件：

- `api/src/web/mod.rs`
- `api/src/web/templates.rs`
- `api/templates/apps.html`
- `api/templates/app_detail.html`
- `api/templates/artifacts.html`
- `api/templates/tasks.html`

测试文件：

- `api/src/web/mod.rs`
- `e2e/tests/smoke.rs`

测试场景：

- 应用创建页不再出现 Binary 模式切换
- 应用详情页可读出发布来源、发布策略、队列和任务层级日志
- 发布版本页支持按应用查看版本与队列状态
- 阶段日志默认折叠，展开后能看到原始输出

### U5：OpenAPI 收缩与文档更新

目标：

- 对外只保留版本投递接口，并把文档说明写完整

主要文件：

- `api/src/web/mod.rs`

测试文件：

- `api/src/web/mod.rs`

测试场景：

- `openapi.json` 中只保留版本投递接口
- 在线文档公开可访问
- 错误响应能清楚说明包名不规范、应用不存在、版本冲突等问题

### U6：旧 Binary 链路退场

目标：

- 删除应用级 Binary 主线，避免后续代码继续分叉

主要文件：

- `api/src/apps.rs`
- `api/src/runtimefs.rs`
- `api/src/web/mod.rs`
- `api/src/web/templates.rs`
- `api/src/maintenance.rs`

测试文件：

- `api/src/web/mod.rs`
- `e2e/tests/smoke.rs`

测试场景：

- Binary 页面和 Binary API 不再可见
- 版本中心使用 `app_releases` 而不是 `binary_artifacts`
- 旧数据存在时不会影响 Compose-only 主流程

## 风险与待定项

### 1. 版本包内部结构不统一

本轮刻意把版本包定义为 opaque package，降低平台复杂度。代价是：

- 平台无法对包内业务结构做强校验
- 脚本质量会直接影响发布稳定性

这是有意的产品取舍。后续如果多个项目收敛出稳定结构，再考虑增加“发布包预设规范”。

### 2. 蓝绿切流仍依赖应用脚本

这会牺牲一部分开箱即用程度，但能显著避免平台内建过多代理模板逻辑。对于本项目当前阶段，这是更合理的平衡点。

### 3. 回滚只回到“旧版本 + 旧配置快照”

本轮回滚不承诺恢复：

- 业务数据库内容
- 外部挂载目录中的任意副作用

这必须在页面与文档里写清楚，避免误导。

### 4. 多节点蓝绿的切流粒度

本轮建议保持简单：

- 发布任务在应用维度串行
- 节点内部按现有滚动策略处理
- 蓝绿切流先不做跨节点原子切换承诺

## 验证与交付要求

实施阶段的基础验证命令：

- `cargo fmt --all --check`
- `cargo check --workspace`
- `cargo test -p api`
- `cargo test -p e2e --test smoke -- --nocapture`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo run -p api -- migrate status`
- `cargo run -p api -- migrate guard`

## 结论

这次重构不是“修补现有部署架构”，而是一次明确的产品收敛：

- 从“双部署主线”收敛到“Compose 单主线”
- 从“外部控制平台配置”收敛到“外部只投递版本包”
- 从“单条部署命令”升级到“可观察的脚本化发布流水线”
- 从“Binary artifact 页面”收敛到“统一发布版本中心”

如果按这份方案推进，后续实现重点会非常明确：

1. 先补 schema 和 release 队列
2. 再补任务阶段模型和脚本流水线
3. 再切 UI 与 OpenAPI
4. 最后删除 Binary 旧链路
