---
title: 多部署单元应用版本与固定流水线开发计划
type: feat
status: active
date: 2026-07-11
origin: docs/brainstorms/2026-06-23-compose-only-deployment-requirements.md
deepened: 2026-07-11
---

# 多部署单元应用版本与固定流水线开发计划

## Summary

在保留现有 Compose 执行器、任务阶段/步骤、制品存储和单应用兼容性的基础上，把应用升级为“环境 + 部署单元 + 不可变应用版本 + 固定流水线”模型。实现按数据库兼容迁移、领域服务拆分、OpenAPI、部署编排、后台页面、公开接入文档和完整验收依次落地。

---

## Problem Frame

现有 `app_releases` 一条记录同时表达版本包和应用版本，`apps.environment`、`app_targets`、`app_runtime_states` 又把环境和运行状态直接挂在应用上。这使一个项目只能自然表达一个 Compose 发布单元，无法让运维一次发布多个有依赖顺序的模块，也无法安全地把测试、正式环境放在同一个项目应用内（见 origin）。

---

## Requirements

- R1. 一个应用可以包含多个相互隔离的环境，每个环境独立维护节点、配置、运行状态、部署锁和历史。
- R2. 一个应用可以包含多个必需或可选部署单元，并通过固定的阶段流水线编排；阶段间串行、阶段内最多三个单元并行。
- R3. 部署单元版本必须把模块 `version`、平台生成的 `versionCode`、SHA-256 和唯一发布包原子绑定，支持本地 multipart 与 OSS 直传完成登记。
- R4. CI 可以基于历史应用版本提交变化项，平台展开并保存包含单元结构、单元版本和各环境配置版本的完整不可变应用版本。
- R5. OpenAPI 只允许上传部署单元版本和创建应用版本，禁止修改配置、流水线或触发部署。
- R6. 运维从应用列表选择环境和应用版本，发起正常部署或强制全量部署；同一环境只能存在一个活动部署。
- R7. 正常部署只跳过制品、配置、脚本、节点、运行状态和健康状态全部与目标一致的单元；全量部署重新执行全部启用单元。
- R8. 部署失败不自动重试；同阶段已开始单元完成后停止后续阶段，并按成功、部分失败、全部失败或已取消汇总。
- R9. 当前过程实时展示阶段、单元、步骤和日志；历史保存结构化结果、不可变快照和受上限约束的完整日志。
- R10. 日志、快照、应用版本和发布包支持引用保护、空间预估、人工清理和审计；未完成上传临时资源自动过期。
- R11. 后台在 AI Token 下方增加“部署接入”，独立免登录文档覆盖配置、脚本、版本包、OpenAPI 和多模块示例。
- R12. 已有 Compose 应用和版本无须重建，迁移后成为默认环境、默认部署单元和单阶段流水线。

---

## Scope Boundaries

- 不恢复 Binary + systemd 发布能力，也不为新模型扩展 Binary 路径。
- 不允许 OpenAPI 创建应用、修改配置、触发部署或读取敏感配置。
- 不在部署时自动拼接各部署单元最新包。
- 不自动重试失败部署，不提供通用数据库 migration 回滚。
- 停用部署单元只停止目标服务，不删除数据库、Docker volume 或共享数据。
- 首版不实现跨应用流水线；所有阶段和单元必须属于同一个应用。

### Deferred to Follow-Up Work

- 对象存储生命周期策略联动：首版继续由平台引用检查和人工清理控制，后续再对接 OSS 生命周期规则。
- 多节点同一部署单元的滚动批次策略：首版复用现有节点执行策略，一个单元内仍按现有节点顺序执行。

---

## Context & Research

### Relevant Code and Patterns

- `api/migrations/0038_compose_release_queue.sql`：现有 `app_releases`、发布队列、制品与任务关联以及应用级活动任务约束。
- `api/migrations/0039_task_phases.sql`：阶段和步骤层级可以直接承载应用阶段、部署单元和脚本步骤。
- `api/migrations/0043_artifact_storage_uploads.sql` 至 `api/migrations/0046_oss_release_integrity_states.sql`：OSS 预约、完成登记、不可覆盖和对象完整性状态。
- `api/src/apps.rs`：现有应用 CRUD、配置快照、发布包上传、发布调度、Compose 队列、恢复中断任务和测试集中在此文件。
- `api/src/tasks.rs`：任务、阶段、步骤和日志查询写入模式。
- `api/src/runtimefs.rs`：应用运行文件落盘、配置哈希和目标节点同步约定。
- `api/src/deploy.rs`：本地/SSH Compose 和脚本命令执行抽象。
- `api/src/web/mod.rs`：应用、制品、部署确认、OpenAPI 和公开文档路由。
- `api/templates/apps.html`、`api/templates/app_detail.html`、`api/templates/deploy_confirm.html`：现有服务端渲染交互模式。
- `api/src/auth/permissions.rs`：权限注册表由代码同步，适合增加部署配置、全量部署和清理权限。
- `e2e/src/lib.rs`：现有应用创建、部署互斥、任务过程、角色权限和 Compose 执行 smoke。

### Institutional Learnings

- `docs/solutions/` 当前没有可复用条目；本次完成后应沉淀多部署单元兼容迁移和应用版本快照经验。
- 历史 migration 不修改；本次只追加 `0047` 及后续补丁 migration，并同时执行 migration status 与 guard。

### External References

- 本计划不依赖新的外部框架或协议，继续使用项目现有 SQLite、Axum、SQLx、Askama、Docker Compose 和 OSS 签名上传实现。

---

## Key Technical Decisions

- 保留 `apps` 作为项目顶层：新增环境、部署单元和配置版本表，不把每个环境复制成独立应用。
- 将不可变 manifest 与部署状态彻底分离：`app_releases` 只保存应用版本身份、完整 manifest、归档状态和 hash；`app_release_queue`、`deployment_runs` 和单元结果表保存每次可变执行状态。迁移完成后任何 worker 都不得再把 `app_releases.status` 改为 deploying/failed。
- 新增 `deployment_unit_releases` 与 `app_release_units`：前者原子绑定模块版本和制品，后者保存应用版本展开后的完整单元目标状态。
- 新增显式配置版本：配置草稿不允许被应用版本引用，发布后的结构/环境/流水线配置以 JSON 快照和哈希固化。
- 发布执行以环境为互斥范围：重建 app 级 queue 索引、operation task trigger、服务层 active 查询和恢复扫描，数据库唯一索引与事务检查共同阻止同一环境并发部署，不阻止同一应用不同环境并行。
- 编排器使用数据库快照生成执行计划：应用版本创建后不再读取可变草稿；部署开始后也不因后台配置变化改变计划。
- 命令执行器改为流式读取 stdout/stderr，在进程输出进入内存时即执行跨 chunk 脱敏和有界头尾缓冲；每步骤默认 10 MiB、每次部署默认 100 MiB，超限继续执行并标记截断。
- 运行真实性由最后成功部署指纹、Compose 容器状态、版本 label 和健康检查共同判断；无法证实时不跳过。
- 旧版本结构回退按目标快照执行，停用单元只运行停止动作；持久化数据永不隐式删除。
- 敏感配置使用进程外主密钥和带版本 AEAD 密文保存：数据库备份只有配套主密钥才可恢复；后台和历史 API 仅返回掩码/指纹，密钥缺失、解密或脱敏初始化失败时拒绝部署。
- 任意部署脚本属于高危可执行配置：脚本编辑/发布使用独立权限，发布前显示 diff 并二次确认；固定非交互 shell 和节点部署用户，禁止平台隐式 sudo，变量通过进程环境传入，工作目录 canonicalize 后必须位于应用根目录。
- 代码拆分领域边界：`apps.rs` 保留兼容门面和旧 Compose 操作，新版本、配置、编排和保留策略进入独立模块。

---

## Open Questions

### Resolved During Planning

- 环境是否拆成独立应用：否；一个项目应用包含多个环境，状态和锁按环境隔离。
- 纯配置变更是否创建应用版本：是；模块制品可以继承，但必须引用新的已发布配置版本。
- CI 是否能触发部署：否；部署始终由后台运维手动确认。
- 部署单元结构变化如何回退：应用版本保存完整结构，回退时预览并恢复目标结构，停用不删除数据。

### Deferred to Implementation

- 现有 runtime 文件目录向环境/单元目录迁移时的精确兼容路径：实现时以 `RuntimeFs` 当前路径测试为准，保留旧默认环境路径读取回退。
- Docker Compose 版本 label 的注入方式：实现时根据当前 YAML 渲染边界选择 override 文件或环境变量，不能改写业务上传包。
- 主密钥具体 AEAD crate 与密文 envelope 编码：实现时选择维护活跃的 RustCrypto AEAD 实现；envelope 必须包含 schema version、key id 和 nonce，测试固定 round-trip、篡改拒绝及轮换。

---

## Output Structure

    api/
    ├── migrations/
    │   ├── 0047_application_environments_and_units.sql
    │   ├── 0048_backfill_default_application_structure.sql
    │   ├── 0049_unit_and_application_release_manifests.sql
    │   └── 0050_environment_deployment_runs.sql
    ├── src/
    │   ├── apps.rs
    │   ├── application_config.rs
    │   ├── application_releases.rs
    │   ├── deployment_orchestrator.rs
    │   ├── deployment_retention.rs
    │   ├── secret_config.rs
    │   ├── runtimefs.rs
    │   ├── tasks.rs
    │   └── web/
    │       ├── mod.rs
    │       └── templates.rs
    └── templates/
        ├── apps.html
        ├── app_detail.html
        ├── app_form.html
        ├── deploy_confirm.html
        ├── deployment_access.html
        └── deployment_history.html

---

## High-Level Technical Design

> 下面的数据流用于确认实现边界，是方向说明，不是要求逐字实现的代码规范。

```text
配置维护
  配置草稿 -> 校验 -> 不可变配置版本

CI 发布
  上传部署单元版本 + 发布包
       -> unit_release_id
  创建应用版本(base + changes + environment config revisions)
       -> 展开完整快照 -> manifest_hash -> ready

运维部署
  选择环境 + 应用版本 + normal/force
       -> 获取环境锁
       -> 比较当前单元指纹与目标指纹
       -> 固化 deployment plan
       -> 按阶段执行部署单元
       -> 写入阶段/单元/步骤/日志
       -> 汇总环境运行状态和部署状态
       -> 释放环境锁
```

核心状态关系：

```text
application
├── environments
│   ├── targets
│   ├── current release/config
│   └── runtime states by unit/node
├── deployment units
├── config revisions
├── unit releases
└── application releases
    ├── complete unit manifest
    └── environment config bindings
```

---

## Implementation Units

### U1. 兼容数据库模型与数据回填

**Goal:** 通过可独立验证的追加 migration 建立新表、约束和兼容回填，使旧应用在迁移后立即表现为一个默认环境、一个默认部署单元和一个单阶段流水线。

**Requirements:** R1, R2, R4, R12

**Dependencies:** None

**Files:**
- Create: `api/migrations/0047_application_environments_and_units.sql`
- Create: `api/migrations/0048_backfill_default_application_structure.sql`
- Create: `api/migrations/0049_unit_and_application_release_manifests.sql`
- Create: `api/migrations/0050_environment_deployment_runs.sql`
- Modify: `api/src/migrations.rs`
- Modify: `api/src/maintenance.rs`
- Test: `api/src/migrations.rs`
- Test: `api/src/apps.rs`

**Approach:**
- `0047` 新增 `app_environments`、`app_environment_targets`、`deployment_units`、流水线阶段、配置草稿/版本和 version counter；不切换现有读写。
- `0048` 回填旧应用结构并做行数、外键和唯一性断言；每个旧应用生成 default 环境、default 单元和单阶段流水线。
- `0049` 新增 `deployment_unit_releases`、不可变应用 release manifest、unit/environment 关联、上传幂等记录和制品清理 tombstone 字段；把旧制品复制为 default unit release，并把旧 `app_releases` 状态迁移为不可变 identity/archive 状态。
- `0050` 新增环境部署运行和单元结果，重建 `app_release_queue` 环境唯一索引、`operation_tasks` environment 关联及 active trigger；旧按 app_id 的 active 查询与启动恢复代码同时切换。
- 每个 migration 都能单独执行和检查，记录回填行数；正式升级前用生产备份副本测量每一步锁库时间。
- 使用原 `environment`、targets、runtime state 和配置快照回填默认环境；旧 `app_releases` 的制品复制为默认单元版本并建立完整关联。
- 明确旧状态迁移矩阵：received/queued/deploying/deployed/failed 等历史值转换为 application release ready/archived identity 与 deployment run/queue 状态，迁移后不再反写 manifest 状态。
- 更新 demo 清理和测试数据库清理顺序，避免外键残留。

**Execution note:** migration-first；先写从旧 schema 升级的失败测试，再创建 migration。

**Patterns to follow:**
- `api/migrations/0042_release_queue_scheduled_status.sql` 的 SQLite 重建表和索引恢复方式。
- `api/src/migrations.rs` 的历史 checksum 与 guard 测试。

**Test scenarios:**
- Integration：空库执行全部 migration 后新表、索引和约束完整。
- Integration：包含旧应用、targets、runtime state、配置快照和 release 的 `0046` 数据库升级后生成默认环境/单元/关联且历史制品未丢失。
- Integration：升级后旧 `/packages` 响应保持兼容，它只为 default unit 创建 unit release 和一个单单元 ready 应用版本，不再自动入队；部署仍由运维手动触发。
- Error path：同一环境插入两个活动部署时数据库拒绝。
- Error path：旧 app 级唯一索引、trigger 和 active 查询均已移除或替换，两个不同环境可以同时持有活动部署。
- Edge case：没有 target 或 release 的 draft 应用仍能安全回填。
- Integration：`clean-demo-data` 能按外键顺序清理新增业务表。

**Verification:**
- `cargo run -p api -- migrate status` 显示 applied/pending/changed/dirty/missing 全部符合预期。
- 历史 migration 文件无 diff，`migrate guard` 只识别新增 migration。

---

### U2. 配置草稿、配置版本与应用结构领域服务

**Goal:** 提供应用环境、部署单元、阶段流水线和配置草稿的校验/发布能力，并生成不可变、脱敏且可比较的配置版本。

**Requirements:** R1, R2, R4, R7, R12

**Dependencies:** U1

**Files:**
- Create: `api/src/application_config.rs`
- Create: `api/src/secret_config.rs`
- Modify: `api/src/lib.rs`
- Modify: `api/src/settings.rs`
- Modify: `api/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `api/src/apps.rs`
- Modify: `api/src/runtimefs.rs`
- Test: `api/src/application_config.rs`
- Test: `api/src/runtimefs.rs`

**Approach:**
- 将环境、单元、阶段、环境目标、Compose、脚本、健康检查和环境变量解析为类型化草稿，统一执行标识唯一性、阶段引用、必需单元、节点和工作目录校验。
- 发布配置时用计数器行在 SQLite 写事务中分配 revision，从草稿生成规范化 JSON、manifest hash、敏感字段指纹和带版本密文。
- 新增必填的进程外 `APP_CONFIG_MASTER_KEY`（或等价 key file）设置；使用随机 nonce 的 AEAD envelope 保存 secret 原文，envelope 记录 key id/schema version。轮换通过新 key 写入、新旧 key ring 解密和维护命令重加密完成，备份 runbook 必须同时备份 key。
- 部署时从不可变配置 revision 解密并写目标节点独立 `.env`；页面、历史 API、审计和普通日志永不返回 secret 原文。密钥缺失、密文篡改、旧 key 不可用或脱敏器无法初始化时 fail closed，拒绝创建部署。
- 脚本配置保存 hash、编辑者和发布者；发布配置前展示脚本 diff 并要求独立 `deployment_scripts.publish` 权限。运行时使用固定非交互 shell、节点部署用户，禁止隐式 sudo；所有 `EASY_DEPLOY_*` 值通过 process env 传递而非拼接 shell 命令。
- 工作目录和包解压目录 canonicalize 后必须位于应用/环境/单元根目录，不能通过软链接越界。
- `RuntimeFs` 新增 app/environment/unit 层级路径，同时对迁移后的 default 环境保留旧路径读取兼容。
- 为配置差异输出部署单元级 fingerprint，供正常部署计划使用。

**Execution note:** test-first；先覆盖规范化、脱敏和结构校验边界。

**Patterns to follow:**
- `api/src/apps.rs` 的 `RuntimeConfigSnapshotInput`、配置 hash 和 YAML 校验。
- `api/src/runtimefs.rs` 的原子写入和路径限制。

**Test scenarios:**
- Happy path：多环境、五单元、三阶段草稿发布为 revision 100，敏感值只出现在加密运行配置中。
- Happy path：旧 key 密文在 key ring 中可解密，新 revision 使用新 key id；重加密后不再依赖旧 key。
- Error path：主密钥缺失、长度错误、密文篡改、未知 key id 或脱敏器初始化失败时拒绝部署且不输出明文。
- Security：无脚本发布权限不能发布包含脚本变化的配置；执行参数含 shell 元字符时仍作为环境值传递且不能改变命令结构。
- Edge case：相同规范配置重复发布返回原 revision，不制造无意义版本。
- Error path：重复 unit key、空阶段、跨应用单元、缺失必需 target、非法 Compose 或绝对路径越界被拒绝。
- Error path：草稿 ID 不能直接被应用版本引用。
- Integration：旧 default 环境读取原 runtime 文件并在首次保存后迁移到新目录。

**Verification:**
- 配置版本可以稳定计算相同 hash，历史返回值不包含 secret 原文。

---

### U3. 部署单元版本与应用版本服务

**Goal:** 原子登记模块制品、平台分配双层 versionCode，并基于基础版本创建完整不可变应用版本。

**Requirements:** R3, R4, R5, R10

**Dependencies:** U1, U2

**Files:**
- Create: `api/src/application_releases.rs`
- Modify: `api/src/lib.rs`
- Modify: `api/src/apps.rs`
- Modify: `api/src/artifact_storage.rs`
- Test: `api/src/application_releases.rs`
- Test: `api/src/artifact_storage.rs`

**Approach:**
- 把现有上传校验抽成部署单元版本服务；multipart 在文件安全落盘和校验完成后事务创建记录，失败时无可用版本。
- OSS 上传会话从申请时绑定 app/unit/version/checksum，complete 校验对象不可覆盖、大小和 checksum 后原子创建 unit release。
- 使用 scope-local `version_counters` 行在 SQLite immediate/write transaction 内分配从 100 开始的单元 versionCode 和应用 versionCode；保证唯一、单调，允许事务失败产生空洞，不承诺绝对连续。
- 应用版本创建接受 base release、unit changes 和各环境 config revision；展开继承并校验完整性后保存 unit/environment 关联与 manifest hash。
- 完整 manifest 使用 immutable ID 引用 unit release，不在部署时按 version 文本重新查包。
- 新增持久化 idempotency 记录，key 按 token + endpoint + action 作用域保存 request hash、resource ID、响应状态和过期时间；同 key 异内容返回冲突。
- 为现有单 release API 保留兼容入口：路由到 default 单元上传并原子创建对应的单单元 ready 应用版本，但不再自动入队；公开文档标记行为变化和兼容字段。
- direct 与 OSS 共用制品安全策略：请求体/压缩包/解压总量/文件数/单文件上限；拒绝 symlink、hardlink、device 和路径越界；在隔离临时目录校验后原子移动。

**Execution note:** test-first，特别覆盖并发、幂等和 OSS 完成登记失败清理。

**Patterns to follow:**
- `api/src/apps.rs` 的 package name 解析、SHA-256、压缩包路径穿越保护和重复上传测试。
- `api/src/artifact_storage.rs` 的 OSS 签名、版本固定和 forbid-overwrite 校验。

**Test scenarios:**
- Happy path：五个 unit release 上传后基于它们创建完整应用版本，双层 versionCode 均从 100 开始。
- Happy path：只更新 admin 并基于历史版本创建新应用版本，最终 manifest 继承其余四个单元。
- Edge case：相同版本、相同 SHA 和相同 Idempotency-Key 返回原记录。
- Error path：相同版本不同 SHA、跨应用 unit_release_id、未 ready 制品、缺失必需单元、草稿配置、未配置新环境均被拒绝。
- Concurrency：两个不同版本并发上传/创建时 versionCode 唯一且单调排序，允许空洞。
- Security：压缩炸弹、超文件数、超单文件、symlink/hardlink/device、路径穿越和超配额包均在 ready 记录创建前拒绝并清理临时目录。
- Idempotency：同 key 同内容可重放原响应；同 key 异内容返回 409；首次成功但客户端超时后重试不产生第二条资源。
- Integration：OSS complete 失败不创建 unit release，清理队列最终删除预约对象。

**Verification:**
- 任意 ready 应用版本都能仅靠关联表还原完整单元和环境目标状态。

---

### U4. 环境级增量计划与固定流水线编排器

**Goal:** 根据应用版本快照和环境当前状态生成可预览的 normal/force 计划，并按阶段执行、停止和汇总。

**Requirements:** R2, R6, R7, R8, R9

**Dependencies:** U1, U2, U3

**Files:**
- Create: `api/src/deployment_orchestrator.rs`
- Modify: `api/src/lib.rs`
- Modify: `api/src/apps.rs`
- Modify: `api/src/deploy.rs`
- Modify: `api/src/tasks.rs`
- Test: `api/src/deployment_orchestrator.rs`
- Test: `api/src/tasks.rs`

**Approach:**
- 计划输入只使用目标应用版本、环境和当前 unit runtime state，输出 deploy/skip/start/stop/upgrade/downgrade/restore 及原因。
- normal 模式要求 artifact/config/script/target fingerprint 相同且容器版本 label、运行状态、健康检查均可信才 skip；无法探测或不健康时执行。
- force 模式执行全部启用单元，但不会启动目标快照中 disabled 单元。
- 创建部署执行时事务获取 environment 活动锁、固化计划和快照，并创建 task/phases/unit-results/steps。
- 阶段按序执行；同阶段使用 Semaphore 控制默认并发 3；失败后等待已启动 future 收口，将未启动后续单元标记未执行。
- 流水线节点支持 `unit` 与 `application_check` 两类；整体检查可以是 HTTP/TCP/脚本 gate，拥有独立超时、步骤日志和失败汇总。
- 停用单元在应用版本中保留 removal stage/order；默认按旧流水线逆序停止并在目标启用阶段前执行，计划预览固化顺序，禁止实现时临时猜测。
- 单元执行复用现有 pre-deploy/deploy/post-deploy/switch/cleanup、Compose 同步和健康检查步骤，脚本接收统一 `EASY_DEPLOY_*` 变量。
- 重构 `CommandRunner` 为增量读取 stdout/stderr 的 streaming API；输出 chunk 先跨边界脱敏，再通过 channel 写 step log 和有界内存 head/tail buffer，禁止 `wait_with_output()` 承载用户脚本输出。
- 取消对运行子进程先发送温和终止、宽限期后强制结束；单元标记 canceled_unknown，不执行回滚。
- 环境和单元状态只在单元成功后更新；部分失败保留已成功单元真实当前状态。
- 控制台重启把 running 单元和环境转为 `reconciling/unknown` 并保留环境锁。只有远端执行租约/fencing token 证明旧进程终止，或具备独立权限的运维确认旧执行已停止并审计解锁后，才允许新部署。
- 部署状态权威来源为 environment deployment run；task 维持 queued/running/success/failed/canceled 兼容状态，unit result 承载 skipped/not_started/canceled_unknown，环境 run 汇总 success/partial_failed/all_failed/canceled。
- 状态优先级固定：仍运行 > canceled_unknown > failure > success；存在成功且存在 failure/canceled/not_started 为 partial_failed，零成功且 canceled_unknown 为 canceled，零成功且失败为 all_failed。

**Execution note:** characterization-first；先固定现有 Compose 单单元任务行为，再实现多单元计划与并发。

**Patterns to follow:**
- `api/src/apps.rs` 的 `ComposeTaskQueue`、`ReleaseDispatchQueue`、中断恢复和 runtime state best-effort 更新。
- `api/src/tasks.rs` 的 phase/step/log 查询模型。

**Test scenarios:**
- Happy path：worker -> api -> 三前端按阶段执行，第三阶段最大并发不超过 3，全部成功汇总绿色 success。
- Happy path：只有 admin fingerprint 改变时 normal 只执行 admin，其余 skipped；force 执行全部启用单元。
- Edge case：目标版本全一致且健康时全部 skipped，部署仍 success。
- Error path：api 失败后同阶段已开始单元完成，后续阶段不启动，结果按成功数汇总 partial/all failed。
- Error path：版本一致但容器停止、label 不同、健康失败或探测未知时不得 skip。
- Concurrency：同环境第二次部署被拒绝并返回活动任务；不同环境可以同时创建部署。
- Cancellation：取消运行脚本后记录 canceled_unknown，后续阶段未执行且无自动回滚。
- Recovery：进程重启后环境保持 reconciling 锁；未确认旧执行终止前第二次部署被数据库和服务层共同拒绝。
- Integration：整体健康检查成功才完成环境部署；整体检查失败形成 partial/all failed 且环境运行状态按单元真实结果保留。
- Integration：超大流式输出不会让内存随总输出增长，日志实时可见且 secret 跨 chunk 边界仍被脱敏。
- Edge case：新增、停用和恢复单元混合时按固化 removal order 执行，中途失败后未执行动作准确记录。

**Verification:**
- 计划预览与实际创建的不可变计划一致，执行期间修改配置草稿不会改变任务。

---

### U5. 有界日志、历史结果与引用安全清理

**Goal:** 保存完整可观察过程，同时限制单步骤/单任务占用，并提供独立的日志、快照、应用版本和未引用制品清理能力。

**Requirements:** R9, R10

**Dependencies:** U1, U4

**Files:**
- Create: `api/src/deployment_retention.rs`
- Modify: `api/src/lib.rs`
- Modify: `api/src/tasks.rs`
- Modify: `api/src/apps.rs`
- Modify: `api/src/platform.rs`
- Test: `api/src/deployment_retention.rs`
- Test: `api/src/tasks.rs`

**Approach:**
- 日志写入器维护每步骤 2 MiB head + 8 MiB tail，并由任务级原子预算器分配总计 100 MiB；任务预算耗尽后各步骤只更新 dropped 计数和有限 tail，不再追加无界 DB/内存内容。UI 分别显示 step/task 截断原因。
- 日志写入前按当前环境敏感值集合和通用 token/password pattern 脱敏。
- 历史单元结果单独保存 status、stage、exit code、failure kind、短错误原因和时间，删除日志不影响结果。
- 清理服务采用 tombstone 状态机 `active -> deleting -> deleted/delete_failed`：事务内锁定并二次校验引用、写 deleting，提交后删除文件/OSS，对象成功后再事务清除元数据；失败保留引用和错误。恢复任务处理“对象已删、数据库尚未收口”的崩溃窗口。
- 所有新应用版本和部署计划拒绝引用 deleting/deleted 制品。
- 应用版本默认 archive；彻底删除检查环境当前/期望版本、活动任务、deployment plan/history、审计记录、base release 和配置版本引用。
- unit artifact 默认被历史部署快照引用时也不可删除，以维持可重放承诺；若运维先删除完整快照解除引用，历史页标记“制品已清理，不可重放”。
- 自动清理仍只处理 24 小时未完成上传会话和临时对象。

**Execution note:** test-first，使用小预算模拟 head/tail 截断和清理失败恢复。

**Patterns to follow:**
- `api/src/artifact_storage.rs` 和现有 release upload cleanup queue 的先验证、后提交删除状态模式。
- `api/src/platform.rs` 的平台设置校验和默认值。

**Test scenarios:**
- Happy path：小日志完整保存；超限后保留头尾、标记截断并记录丢弃字节。
- Error path：日志包含 token/密码/环境 secret 时查询结果无原文。
- Happy path：删除日志后任务、阶段、单元结果和失败摘要仍可查。
- Error path：当前运行、被应用版本引用或活动任务使用的制品无法删除并返回引用列表。
- Error path：OSS 删除失败时数据库引用仍在，下一次清理可安全重试。
- Recovery：对象删除成功后、数据库收口前进程崩溃，恢复任务能确认对象不存在并完成 deleted 状态。
- Integration：清理预览返回按日志/快照/制品/临时文件分类的预计空间。

**Verification:**
- 任意清理操作都有审计记录，且不会产生指向不存在制品的 ready 应用版本。

---

### U6. OpenAPI 与公开接口文档

**Goal:** 提供部署单元包上传和应用版本创建接口，保持旧上传调用兼容，并让错误响应足以指导 CI/AI 修正。

**Requirements:** R3, R4, R5, R11

**Dependencies:** U2, U3

**Files:**
- Modify: `api/src/web/mod.rs`
- Modify: `api/src/auth/service.rs`
- Modify: `api/src/web/templates.rs`
- Test: `api/src/web/mod.rs`
- Test: `e2e/src/lib.rs`

**Approach:**
- 新增 unit release multipart、OSS upload create/complete、application release create 路由，使用稳定 ID 和 Idempotency-Key。
- API Token 增加 app/unit resource scope、过期时间、撤销、最后使用时间和并发/速率配额；默认最小范围。审计只记录 token ID/prefix，绝不记录原文。
- 应用版本接口接受 base application release、changes、environment config revisions，并返回展开后的完整摘要。
- API Token 只需 artifact upload/application release create 权限，不暴露部署操作。
- OpenAPI 3.1 文档描述版本格式、冲突、缺单元、配置 revision、OSS 完成登记和幂等错误。
- 旧 `/api/v1/services/{service_key}/packages` 映射 default unit；响应增加 deprecation 信息但不破坏现有字段。

**Execution note:** test-first；先写路由权限、幂等和完整性失败测试。

**Patterns to follow:**
- `api/src/web/mod.rs` 现有 direct/OSS package API 和公开 `openapi_spec()`。

**Test scenarios:**
- Happy path：Token 上传五个单元包并创建应用版本，响应包含 unit/app versionCode 和 manifest hash。
- Error path：无权限、错误 app/unit、错误 SHA、缺必需单元、非法 semver、不同内容幂等冲突返回稳定 code。
- Security：Token 跨 app/unit scope 上传或创建版本被拒绝；过期/撤销/超并发或速率限制返回稳定错误。
- Integration：旧 service package API 仍可给迁移后的 default unit 上传。
- Security：任何 OpenAPI 路由都不能触发 deployment run 或读取环境 secret。
- Documentation：公开 spec 和 HTML 包含新流程并互相链接部署接入文档。

**Verification:**
- 仅依据公开文档可以完成从模块包上传到 ready 应用版本创建，且没有部署接口。

---

### U7. 应用创建、配置和部署运维界面

**Goal:** 把应用列表变成一次部署入口，并提供环境/单元/流水线配置、计划确认、实时过程和结构化历史页面。

**Requirements:** R1, R2, R6, R7, R8, R9, R10

**Dependencies:** U2, U3, U4, U5

**Files:**
- Modify: `api/src/web/mod.rs`
- Modify: `api/src/web/templates.rs`
- Modify: `api/templates/apps.html`
- Modify: `api/templates/app_detail.html`
- Modify: `api/templates/app_form.html`
- Modify: `api/templates/deploy_confirm.html`
- Create: `api/templates/deployment_history.html`
- Modify: `api/assets/app.js`
- Modify: `api/assets/app.css`
- Modify: `api/src/auth/permissions.rs`
- Test: `api/src/web/mod.rs`
- Test: `e2e/src/lib.rs`

**Approach:**
- 创建应用采用基本信息 -> 环境 -> 部署单元 -> 流水线 -> 校验启用的分步流程；未发布配置前保持 configuring 状态。
- 应用列表按环境展示运行状态、部署状态、当前/最新版本，并把部署按钮放在主操作区。
- 部署弹窗选择环境、明确版本和 normal/force，异步请求计划预览并展示 deploy/skip/start/stop/upgrade/downgrade 与结构回退警告。
- 计划预览返回不可变 plan ID/hash；确认时重新校验环境、制品、磁盘、节点和健康预检，hash 不一致则拒绝并要求重新预览。提交使用幂等 key，按钮立即禁用防止双击。
- 详情页按环境切换，展示单元运行指纹、当前部署过程、阶段/单元状态和可展开实时日志。
- 应用列表和详情始终提供“查看活动部署”；取消入口使用独立权限和风险确认，明确 canceled_unknown 表示目标状态未知。
- 历史页只默认展示结构化结果；完整日志和配置快照按需展开，并提供有权限的清理预览/确认。
- 状态使用文字 + 图标 + 颜色 + ARIA label：success 绿色、partial/canceled_unknown 橙色、failed 红色、running 蓝色、waiting/skipped 普通灰色、canceled 深灰。禁止只靠颜色传达结果。
- 新增细分权限并让内置角色按现有职责自动获得；强制全量、清理和结构配置不隐式授予只读角色。

**Execution note:** 后端行为测试优先，再实现模板和交互；完成后用浏览器验证桌面与移动布局。

**Patterns to follow:**
- `api/templates/deploy_confirm.html` 的风险摘要、节点预检和确认结构。
- `api/templates/task_detail.html` 的阶段/步骤/日志展开。
- `api/assets/app.js` 的同源 fetch 登录失效跳转处理。

**Test scenarios:**
- Happy path：运维创建多环境五单元应用、发布配置、看到 ready 应用版本并从列表部署。
- Happy path：normal 弹窗准确显示一个 deploy 四个 skip；force 显示全部启用单元 deploy。
- Error path：活动部署期间按钮禁用且服务端再次拒绝；草稿配置或不完整应用版本不可部署。
- State：单元混合成功/失败/未执行时应用显示部分失败和正确颜色；全部失败、取消、全跳过规则正确。
- Permission：viewer 只读；deployer 可 normal；配置管理员可维护草稿；清理和 force 权限独立。
- Responsive：最长应用/单元/版本文本在移动端不溢出，按钮与状态不重叠。
- Accessibility：键盘可完成版本选择、计划确认、日志展开和取消；高对比/色弱下仍能由文字和图标识别状态。

**Verification:**
- 运维完成全量发布只需在应用列表点击一次部署、确认一次计划，不需要逐单元操作。

---

### U8. 部署接入菜单与免登录独立文档

**Goal:** 提供后台简明入口和可被业务项目/AI 直接读取的完整部署规范。

**Requirements:** R11

**Dependencies:** U3, U6, U7

**Files:**
- Create: `api/templates/deployment_access.html`
- Modify: `api/src/web/mod.rs`
- Modify: `api/src/web/templates.rs`
- Modify: `api/src/auth/permissions.rs`
- Modify: `api/assets/app.css`
- Test: `api/src/web/mod.rs`
- Test: `e2e/src/lib.rs`

**Approach:**
- 后台 `/deployment-access` 使用普通布局，放在 AI Token 下方，提供简述和新标签打开按钮。
- 公开 `/docs/deployment` 免登录，使用独立 public view model、可打印和可锚点导航的 HTML 文档，禁止复用后台配置模型。
- 文档覆盖概念、配置草稿/版本、脚本契约、包规范、双层版本、OpenAPI、normal/force、回退、清理、单单元示例和 qfy-voucher-hub 多单元示例。
- 所有示例使用占位 Token、节点和 secret；文档链接 `/docs/openapi`，OpenAPI 文档反向链接部署规范。

**Execution note:** 文档内容由最终实际接口和脚本契约生成后再定稿，避免计划先行造成漂移。

**Patterns to follow:**
- `api/src/web/mod.rs` 的 `openapi_docs_public_html()` 和公开访问测试。
- `api/templates/api_tokens.html` 的独立文档入口按钮。

**Test scenarios:**
- Public：未登录访问 `/docs/deployment` 返回 200，包含新 API 流程且无真实凭据。
- Security：用含诱饵节点、内部路径、secret key/value 和 Token 的测试数据库渲染公开文档与错误响应，快照扫描确认全部不出现。
- Navigation：有权限用户在 AI Token 下方看到部署接入菜单，按钮 `target="_blank"`。
- Permission：无后台页面权限者不显示菜单，但公开文档仍可直接访问。
- Documentation：OpenAPI 与部署文档互相链接；文档示例提取为 fixture 并通过 schema/请求契约测试，能够创建 ready manifest。

**Verification:**
- 新项目 AI 不登录后台即可理解配置契约并生成正确上传/创建版本调用。

---

### U9. 兼容回归、E2E、运维迁移与知识沉淀

**Goal:** 证明旧应用不回归、新多单元主流程端到端可用，并为正式升级提供迁移、备份和回退说明。

**Requirements:** R1-R12

**Dependencies:** U1-U8

**Files:**
- Modify: `e2e/src/lib.rs`
- Modify: `docs/runbooks/api-migrations.md`
- Create: `docs/solutions/multi-unit-application-release-model.md`
- Modify: `README.md`
- Modify: `scripts/deploy-systemd.sh`（仅当 migration/健康验证需要新增参数）

**Approach:**
- 扩展 smoke 覆盖旧单元兼容、新应用配置、模块包上传、应用版本创建、normal/force、部分失败、权限和公开文档。
- 增加 migration fixture，从 `0046` 备份结构升级并断言历史应用/制品/任务可读。
- 更新 migration runbook，列出本次表规模、SQLite migration 时长、正式备份和升级后检查 SQL。
- 沉淀“顶层应用版本 + 单元制品 + 环境状态”模型及引用安全删除经验。
- 正式部署前检查 production 队列为空、备份 SQLite、本地构建 Linux x86_64；部署后验证 migration、服务、健康、日志和旧应用页面。

**Execution note:** 收口单元；完整测试、静态检查、浏览器截图和生产前审计全部通过后才能提交最终功能。

**Patterns to follow:**
- `docs/runbooks/api-migrations.md` 的 status/guard/备份流程。
- `scripts/deploy-systemd.sh` 的低内存服务器本地构建部署约束。

**Test scenarios:**
- E2E：旧单元应用上传兼容包并部署成功。
- E2E：五单元应用只更新 admin 时 normal 仅执行 admin，force 全部执行。
- E2E：中间阶段失败产生部分失败，后续未执行，历史结果和颜色正确。
- E2E：同环境并发阻止、跨环境允许。
- Migration：0046 数据升级后旧详情、制品和历史任务仍可查看。
- Browser：应用列表、详情、部署弹窗、历史和公开文档在桌面/移动视口无重叠和溢出。

**Verification:**
- `cargo fmt --all --check`
- `cargo check --workspace`
- `cargo test -p api`
- `cargo test -p e2e --test smoke -- --nocapture`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `node --check api/assets/app.js`
- `cargo run -p api -- migrate status`
- `cargo run -p api -- migrate guard origin/main`

---

## System-Wide Impact

- **Interaction graph:** OpenAPI 上传创建 unit release，应用版本服务展开配置快照，后台部署创建环境任务，编排器调用 Compose/脚本并写入 task phases/steps/logs，最终更新 unit/environment runtime state；清理服务反向检查这些引用。
- **Error propagation:** 上传/版本创建错误同步返回稳定 API code；部署错误进入单元结果和应用汇总，不自动重试；清理外部对象失败保留数据库引用。
- **State lifecycle risks:** 重点防止半完成上传、应用版本引用缺包、配置草稿竞态、部分成功后错误覆盖环境状态、进程重启遗留锁和删除被引用制品。
- **API surface parity:** direct multipart、OSS create/complete 和旧 service package 三条上传入口必须共享相同领域校验；后台与 OpenAPI 创建应用版本必须共享事务服务。
- **Integration coverage:** 必须使用真实 SQLite、RuntimeFs、任务表和受控 CommandRunner 覆盖从上传到应用版本再到部署结果的完整链路，不能只依赖 mock 单元测试。
- **Unchanged invariants:** 历史 migration 不修改；外部项目不能触发部署；Compose 仍是唯一新应用运行方式；旧单单元应用继续工作；Caddy 和生产部署目标不因该功能自动修改。

---

## Risks & Dependencies

- **SQLite 大型兼容迁移风险：** 使用新增表和回填优先，必须用 0046 fixture 测试；正式执行前备份并检查行数，禁止修改历史 migration。
- **`apps.rs` 过大导致交叉修改风险：** 先抽取新领域模块，以 `AppService` 门面委托，按单元小步提交并持续运行 API 测试。
- **版本与配置双重不可变造成引用复杂：** 所有关联使用 ID + manifest hash，创建应用版本和获取部署锁必须在事务内完成。
- **阶段并行导致任务/日志竞争：** 单元结果使用独立行，日志按 step ID 写入，汇总在同阶段 futures 收口后单线程事务更新。
- **敏感配置泄露：** 历史只存脱敏值/指纹，命令环境和日志写入统一脱敏，公开文档只用占位值。
- **旧 API 调用方回归：** 保留 default unit 兼容适配和旧响应字段，E2E 覆盖现有上传路径。
- **磁盘增长：** 有界日志、空间预览和引用安全人工清理同时落地，不能只实现无限保留。
- **任意脚本 RCE 边界：** 脚本发布权限等价于目标节点部署用户的代码执行权限；独立权限、diff 确认、禁止隐式 sudo、固定用户、路径约束和完整审计必须同时存在。

---

## Phased Delivery

### Gate 1：兼容 schema 与暗写

- U1 只新增/回填新模型并保留旧读路径；新功能开关关闭。
- go/no-go：0046 fixture 升级、旧 `/packages`、旧详情和旧部署 smoke 全绿，生产副本锁库时间可接受。

### Gate 2：单单元新模型

- U2-U3 让 default 环境/default 单元通过新配置和版本服务读写，旧页面仍可回退。
- go/no-go：不可变 manifest、密钥备份恢复、幂等和制品安全测试通过。

### Gate 3：多单元编排

- U4-U5 启用环境锁、固定流水线、流式日志和结构化结果。
- go/no-go：并发 barrier、重启 reconciling、取消和大输出内存有界测试通过。

### Gate 4：OpenAPI 与运维界面

- U6-U7 开放新 CI 接口和一次部署入口，保持兼容 API。
- go/no-go：权限、plan hash、双击幂等和浏览器 smoke 通过。

### Gate 5：清理、公开文档与收口

- U5 清理 UI、U8-U9 文档/E2E/运维说明完成后才默认开启多单元创建。
- go/no-go：引用矩阵、tombstone 崩溃恢复、公开响应泄密扫描和完整回归通过。

---

## Documentation / Operational Notes

- origin 需求是产品行为唯一来源；实现细节变化时同步更新本计划和部署接入文档，不修改已确认的边界。
- 每个实现单元完成、测试通过后按功能边界提交并立即 push `main`。
- 正式环境升级属于后续用户明确部署指令范围；完成功能、提交和推送不自动等同授权部署正式环境。
- 生产升级前必须确认没有运行中的 `operation_tasks`、`app_release_queue` 和新环境部署记录，并保留 SQLite 备份路径。

---

## Sources & References

- **Origin document:** `docs/brainstorms/2026-06-23-compose-only-deployment-requirements.md`
- 现有执行计划：`docs/plans/2026-06-23-compose-only-release-pipeline.md`
- 制品可靠性计划：`docs/plans/2026-07-10-001-fix-release-artifact-reliability-closure-plan.md`
- Migration 规则：`docs/runbooks/api-migrations.md`
- 核心领域：`api/src/apps.rs`
- 任务模型：`api/src/tasks.rs`
- Web 路由与公开文档：`api/src/web/mod.rs`
