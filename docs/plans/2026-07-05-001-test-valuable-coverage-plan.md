---
title: 提高部署平台高价值测试覆盖率任务计划
type: test
status: completed
date: 2026-07-05
origin: docs/plans/2026-07-03-001-feat-qfy-sc-template-alignment-plan.md
---

# 提高部署平台高价值测试覆盖率任务计划

## Summary

当前工作区的 `cargo llvm-cov` 行覆盖率基线为 `31246/39051 = 80.01%`。下一轮测试补强不以“刷未覆盖行”为主，而是优先覆盖部署平台最容易造成线上事故的链路：发布版本、串行队列、配置快照、部署阶段日志、SSH 探测、OpenAPI 投递、RBAC/CSRF 和 E2E 验收辅助能力。

短期目标是把总行覆盖率推进到 `82%+`，同时确保新增测试能真实保护 Compose-only 发布主线。中期目标是把 `api/src/apps.rs`、`api/src/web/mod.rs`、`api/src/deploy.rs`、`api/src/nodes.rs`、`e2e/src/lib.rs` 中仍缺少保护的关键路径补齐。

---

## Problem Frame

覆盖率超过 80% 后，继续提升的性价比取决于测试是否覆盖真实风险。当前剩余缺口集中在部署编排、页面路由和 E2E helper 这类大文件中，如果只补 HTML 文案、列表标签或简单格式化函数，会增加维护成本但不能显著降低发布风险。

本计划把后续测试投入聚焦到“平台能否稳定接收版本包、生成正确发布计划、串行执行、记录可排障日志、拒绝非法操作、保护权限边界”这些核心问题上。

---

## Coverage Baseline

本基线来自本地生成的 `tmp/coverage-summary.json`，该文件是临时产物，不提交到仓库。

重点缺口：

- `api/src/apps.rs`：`7455/11094`，行覆盖率 `67.19%`，未覆盖 `3639` 行。这里承载应用配置、发布版本、队列、快照、部署调度，是第一优先级。
- `e2e/src/lib.rs`：`4508/6300`，行覆盖率 `71.55%`，未覆盖 `1792` 行。这里承载验收测试辅助方法和真实用户流程，是第二优先级。
- `api/src/web/mod.rs`：`7413/8563`，行覆盖率 `86.57%`，未覆盖 `1150` 行。这里承载页面路由、OpenAPI、权限和 CSRF，是第三优先级。
- `api/src/deploy.rs`：`1693/2050`，行覆盖率 `82.58%`，未覆盖 `357` 行。这里承载 SSH、Compose、命令执行和 known_hosts，是高风险运维边界。
- `api/src/nodes.rs`：`1333/1601`，行覆盖率 `83.26%`，未覆盖 `268` 行。这里承载节点探测、安装、能力识别和事件日志。
- `api/src/migrations.rs`：`615/750`，行覆盖率 `82.00%`，未覆盖 `135` 行。这里承载 migration 规范保护和发布前风险控制。
- `api/src/auth/service.rs`：`2262/2393`，行覆盖率 `94.52%`，剩余缺口不大，但涉及认证授权，仍保留少量安全边界补测。
- `api/src/node_credentials.rs`：`480/576`，行覆盖率 `83.33%`，需要补凭据文件、权限、禁用和错误路径。
- `api/src/main.rs`：`134/227`，行覆盖率 `59.03%`，主要是 CLI/启动参数边界，适合做轻量补测。

---

## Requirements

- R1. 新增测试必须优先覆盖部署平台主链路，而不是为了覆盖率补低价值文案或样式断言。
- R2. 发布版本、发布队列、配置快照和部署任务状态必须有单元测试或集成测试保护。
- R3. SSH known_hosts、节点探测、节点安装和事件日志必须覆盖成功、失败和错误摘要。
- R4. Web 后台关键写操作必须覆盖登录态、RBAC、CSRF、非法表单和错误提示。
- R5. OpenAPI 版本包投递必须覆盖 AI/脚本接入常见错误：token、包名、应用归属、重复版本、发布模式。
- R6. E2E 测试要补足真实验收路径，但不能依赖真实 Docker、真实 SSH、真实 systemd 或公网服务。
- R7. 所有测试应使用临时 SQLite、tempdir、fake runner 和本地 HTTP router，保持稳定、可重复、低依赖。
- R8. 每轮测试补强后都要能用统一命令验证覆盖率变化，并且临时覆盖率文件继续放在 `tmp/`。

---

## Scope Boundaries

- 不为纯 UI 文案、按钮标题、颜色 class 或静态 HTML 片段单独补覆盖率，除非它们承载权限、状态或关键操作语义。
- 不引入真实 Docker、SSH、systemd、Caddy、Redis 或外部服务作为测试依赖。
- 不在本计划中重构大模块结构；如果实现测试时必须解耦，只做小范围可测性调整。
- 不修改历史 migration；migration 覆盖只测试规范、guard、status 和错误提示。
- 不把 `tmp/coverage-summary.json`、`tmp/coverage-full.json`、`tmp/coverage-missing.txt` 等临时覆盖率产物提交进仓库。
- 不把覆盖率数字作为唯一验收标准；如果测试暴露真实缺陷，优先修复缺陷。

---

## Context & Research

### Relevant Code and Patterns

- `api/src/apps.rs` 已有发布版本、发布队列、配置快照、发布调度和部分单测，是本轮最大补强点。
- `api/src/tasks.rs` 当前覆盖率较高，可作为任务、阶段、步骤、日志断言的稳定服务层。
- `api/src/deploy.rs` 已有 fake command runner 和 known_hosts 测试，可继续扩展 SSH/Compose 命令构造与错误路径。
- `api/src/nodes.rs` 已有节点探测 known_hosts 测试，可继续补探测输出解析、能力缺失、安装失败和事件日志。
- `api/src/web/mod.rs` 已有 `test_web_app`、登录 helper、OpenAPI 文档测试和部分页面路由测试，可继续补安全边界和关键 POST 路由。
- `e2e/src/lib.rs` 已有真实验收 helper，适合补 helper 自测和端到端 smoke 流程，而不是只补独立工具函数。
- `api/src/migrations.rs` 和 `api/src/main.rs` 适合补轻量 CLI/migration 边界，收益中等。

### Related Plans

- `docs/plans/2026-06-23-compose-only-release-pipeline.md` 定义 Compose-only 发布主线、release/queue/task phase 模型和 OpenAPI 收缩方向。
- `docs/plans/2026-07-03-001-feat-qfy-sc-template-alignment-plan.md` 定义 qfy-sc 模板同构接入、发布计划、脚本阶段和日志观察方向。

---

## Key Technical Decisions

- 以业务风险排序，不以未覆盖行数绝对值排序。`api/src/apps.rs` 缺口最大且风险最高，优先处理。
- 尽量测试服务层和行为语义，不测试脆弱 HTML 排版。Web 测试重点断言状态码、重定向、权限、CSRF、关键内容和数据状态。
- 部署、SSH、节点探测使用 fake command runner；只验证命令构造、状态转换、日志记录和错误传播。
- 发布队列测试优先覆盖串行、失败阻塞、手动/定时发布和自动入队，不做真实时间长等待。
- E2E helper 自测聚焦“测试工具本身可靠”，避免后续 smoke 出错时先怀疑 helper。
- 覆盖率目标先定为 `82%+`，但每个任务必须有明确的风险保护价值。

---

## Task List

### U1. 发布版本包与发布队列状态测试

**Goal:** 补齐版本包接收、版本记录、自动入队、手动发布、定时发布、取消和串行队列的核心测试。

**Priority:** P0

**Requirements:** R1, R2, R5

**Files:**
- Modify/Test: `api/src/apps.rs`
- Modify/Test: `api/src/tasks.rs`
- Optional Test: `api/src/web/mod.rs`

**Tasks:**
- [ ] 覆盖 `auto_queue_release=true`：上传版本包后创建 release 和 queue，队列状态为 `queued`。
- [ ] 覆盖 `auto_queue_release=false`：上传版本包后只记录 release，不创建 queue。
- [ ] 覆盖手动发布：用户选择已有 release 后创建 queue，重复发布同一 release 返回清晰错误。
- [ ] 覆盖定时发布：未到时间不执行，到时间后进入队列。
- [ ] 覆盖取消定时发布：release/queue 状态恢复到可理解状态，不残留 active task。
- [ ] 覆盖同一应用连续上传多个版本：按接收顺序串行执行，不按 `version_code` 插队。
- [ ] 覆盖失败阻塞：当前 release 失败后，后续 release 保持 queued，不自动跳过。

**Test scenarios:**
- Happy path：`orders-api_version_1_2_3.tar.gz` 上传到 `package_upload` 应用，生成 release、queue 和正确 source。
- Happy path：连续上传 `1.2.3`、`1.2.4`、`1.2.5`，队列顺序和接收顺序一致。
- Edge case：上传较小 `version_code` 的晚到版本，不插队到前面。
- Error path：重复版本返回冲突错误，并且不重复落盘。
- Error path：失败队列项阻塞后续队列，人工取消后才允许继续。

**Verification:**
- `cargo test -p api apps::tests:: -- --nocapture`
- 覆盖率中 `api/src/apps.rs` 的 release/queue 相关缺口下降。

---

### U2. 配置快照与发布绑定测试

**Goal:** 确保每次发布绑定当时的 Compose/env/scripts/目标节点配置快照，后续修改配置不会污染已入队或已执行版本。

**Priority:** P0

**Requirements:** R2, R7

**Files:**
- Modify/Test: `api/src/apps.rs`
- Modify/Test: `api/src/runtimefs.rs`

**Tasks:**
- [ ] 覆盖保存 Compose 内容后创建新配置快照。
- [ ] 覆盖保存 env 内容后创建新配置快照。
- [ ] 覆盖保存部署脚本后创建新配置快照。
- [ ] 覆盖目标节点变化后创建新配置快照。
- [ ] 覆盖发布队列绑定快照 ID，后续配置修改不改变已绑定快照。
- [ ] 覆盖回滚或重试使用正确的 release + snapshot 组合。
- [ ] 覆盖不存在的 snapshot/release 返回明确错误。

**Test scenarios:**
- Happy path：应用保存配置 A 后上传 release 并入队，随后保存配置 B，执行队列仍读取配置 A。
- Happy path：手动发布当前配置时生成 deploy snapshot，并能在任务中读取。
- Edge case：env 内容为空但 Compose 有效时仍可生成快照。
- Error path：队列引用不存在 snapshot 时任务失败并记录原因。

**Verification:**
- `cargo test -p api apps::tests:: -- --nocapture`
- `api/src/apps.rs` 中 snapshot、queue、runtime state 相关分支被覆盖。

---

### U3. 部署任务阶段、步骤与失败传播测试

**Goal:** 保护“发布任务 -> 阶段 -> 步骤 -> 原始输出 -> runtime state”的一致性，确保失败能被准确追踪。

**Priority:** P0

**Requirements:** R2, R6, R7

**Files:**
- Modify/Test: `api/src/apps.rs`
- Modify/Test: `api/src/tasks.rs`
- Modify/Test: `api/src/deploy.rs`

**Tasks:**
- [ ] 覆盖 `prepare`、`render`、`preflight`、`pre_deploy`、`deploy`、`post_deploy`、`health_check`、`finalize` 的成功路径。
- [ ] 覆盖 `deploy` 脚本失败时，step、phase、task、queue、runtime state 都进入失败状态。
- [ ] 覆盖健康检查失败时，任务失败并保留部署输出。
- [ ] 覆盖空脚本阶段不失败，且 UI/任务摘要可解释。
- [ ] 覆盖多节点滚动策略：失败即停止和失败后继续两种策略。
- [ ] 覆盖任务取消或重试时，队列状态和旧日志不会混淆。

**Test scenarios:**
- Happy path：fake runner 返回成功，任务最终为 success，阶段全部完成。
- Error path：`deploy` 返回 exit code 1，任务失败，后续阶段不执行或标记 skipped。
- Error path：健康检查超时，任务失败，runtime state 记录最后错误。
- Integration：任务详情能从 task service 读到 phase、step、log 的完整层级。

**Verification:**
- `cargo test -p api tasks::tests:: apps::tests:: -- --nocapture`
- 失败传播不依赖真实 Docker/Compose。

---

### U4. SSH known_hosts 与远程命令测试

**Goal:** 保护正式环境曾经暴露过的 known_hosts 问题，确保 SSH、SCP、docker compose 远程命令都使用平台托管的 known_hosts。

**Priority:** P0

**Requirements:** R3, R6, R7

**Files:**
- Modify/Test: `api/src/deploy.rs`
- Modify/Test: `api/src/nodes.rs`

**Tasks:**
- [ ] 覆盖 known_hosts 父目录不存在时自动创建。
- [ ] 覆盖 known_hosts 文件不存在时自动创建。
- [ ] 覆盖 host 已存在时不重复执行 `ssh-keyscan`。
- [ ] 覆盖 `ssh-keyscan` 失败时返回可理解错误。
- [ ] 覆盖写入 known_hosts 失败时错误包含文件路径。
- [ ] 覆盖 SSH 命令统一追加 `UserKnownHostsFile` 和 `StrictHostKeyChecking=yes`。
- [ ] 覆盖 SCP/rsync/远程 compose 命令同样使用 identity file 和 known_hosts。
- [ ] 覆盖 root、本机回环、非 22 端口的 lookup key 构造。

**Test scenarios:**
- Happy path：首次连接 `10.0.2.11:22` 时执行 `ssh-keyscan -H` 并写入 managed known_hosts。
- Happy path：第二次连接同一 host 不重复写入。
- Edge case：端口为 `2222` 时 known_hosts lookup 使用带端口格式。
- Error path：`ssh-keyscan` 返回非 0，节点探测事件摘要包含 known_hosts 采集失败。

**Verification:**
- `cargo test -p api deploy::tests:: nodes::tests:: -- --nocapture`
- 所有远程命令构造测试不依赖真实 SSH 服务。

---

### U5. 节点探测、节点安装与事件日志测试

**Goal:** 让节点探测失败时能被用户和 AI 通过事件日志快速定位，避免“前端只显示探测失败但不知道原因”。

**Priority:** P1

**Requirements:** R3, R7

**Files:**
- Modify/Test: `api/src/nodes.rs`
- Modify/Test: `api/src/events.rs`
- Optional Test: `api/src/web/mod.rs`

**Tasks:**
- [ ] 覆盖未绑定凭据时探测失败，并记录清晰事件。
- [ ] 覆盖凭据禁用或私钥文件不存在时探测失败。
- [ ] 覆盖 SSH exit code 255 时记录 command、destination、identity_file、combined_output。
- [ ] 覆盖探测输出缺少字段时，能力识别为 missing/unknown 而不是 panic。
- [ ] 覆盖 docker、compose、caddy、nginx 缺失时的能力识别。
- [ ] 覆盖节点安装 Docker/Compose/Caddy 的命令构造与失败记录。
- [ ] 覆盖事件日志一键复制内容包含时间、级别、类型、目标、摘要和详情。

**Test scenarios:**
- Happy path：fake probe 返回 os、disk、systemd、docker、compose，节点能力缓存更新。
- Error path：SSH 返回 `Host key verification failed`，事件日志包含 known_hosts 提示。
- Error path：凭据缺失，探测不执行 ssh 命令，直接返回配置错误。
- Edge case：部分能力命令不存在，节点仍能保存已识别能力。

**Verification:**
- `cargo test -p api nodes::tests:: events::tests:: -- --nocapture`
- 节点探测页面单行加载和事件日志输出都有服务层保护。

---

### U6. Web 后台关键路由安全测试

**Goal:** 补齐高风险后台写操作的登录态、RBAC、CSRF 和非法表单测试，避免 UI 调整破坏安全边界。

**Priority:** P1

**Requirements:** R4, R5

**Files:**
- Modify/Test: `api/src/web/mod.rs`
- Optional Test: `api/src/auth/service.rs`

**Tasks:**
- [ ] 覆盖节点新增、编辑、探测、安装组件的未登录重定向。
- [ ] 覆盖节点相关写操作缺少 CSRF 时拒绝。
- [ ] 覆盖凭据生成、上传、禁用、删除吊销 token 的 CSRF 与权限。
- [ ] 覆盖应用创建、更新配置、保存脚本、绑定节点的非法表单错误。
- [ ] 覆盖发布、定时发布、取消、重试、跳过失败的权限边界。
- [ ] 覆盖任务取消、重试、查看日志的权限边界。
- [ ] 覆盖 API Token 页面刷新不新增 token 的回归测试。

**Test scenarios:**
- Happy path：管理员带正确 CSRF 能提交应用配置。
- Error path：viewer 访问写操作返回 forbidden 或重定向到权限错误。
- Error path：缺少 CSRF 的 POST 不改变数据库。
- Error path：刷新 API Token 页面只读取列表，不产生新 token。
- Integration：页面 POST 后数据状态与 service 查询一致。

**Verification:**
- `cargo test -p api web::tests:: -- --nocapture`
- 新测试断言语义，不断言完整 HTML 排版。

---

### U7. OpenAPI 版本包投递边界测试

**Goal:** 确保外部项目或 AI 只依赖一个版本包投递接口也能稳定接入，并在错误时拿到可执行的错误提示。

**Priority:** P1

**Requirements:** R5, R6

**Files:**
- Modify/Test: `api/src/web/mod.rs`
- Modify/Test: `api/src/apps.rs`
- Optional Test: `api/src/auth/service.rs`

**Tasks:**
- [ ] 覆盖无 token、无效 token、吊销 token、权限不足 token。
- [ ] 覆盖包名不符合 `{service_key}_version_{x_y_z}.tar.gz` 的错误。
- [ ] 覆盖 path 中 `service_key` 与包名中的 service key 不一致。
- [ ] 覆盖 app 不存在、app 不是 `package_upload`、app 被禁用。
- [ ] 覆盖重复版本、重复校验和、空文件、超限文件。
- [ ] 覆盖 auto queue 开启/关闭时返回体中的 release/queue 状态差异。
- [ ] 覆盖 `/docs/openapi` 和 `/openapi.json` 公开可访问，且只描述版本包投递接口。

**Test scenarios:**
- Happy path：有效 token 上传 `orders-api_version_1_2_3.tar.gz`，返回 release id、version、version_code、queued 状态。
- Error path：`orders-api-1.2.3.tar.gz` 返回命名规范错误。
- Error path：token 已吊销，返回 unauthorized，数据库不新增 release。
- Error path：`manual` 应用上传版本包返回业务错误。

**Verification:**
- `cargo test -p api web::tests:: apps::tests:: -- --nocapture`
- 公开 OpenAPI 文档与实际接口行为保持一致。

---

### U8. E2E helper 与 smoke 流程测试

**Goal:** 提高 `e2e/src/lib.rs` 的可靠性，确保验收测试失败时能快速判断是产品问题还是测试 helper 问题。

**Priority:** P1

**Requirements:** R6, R7

**Files:**
- Modify/Test: `e2e/src/lib.rs`
- Modify/Test: `e2e/tests/smoke.rs`

**Tasks:**
- [ ] 覆盖登录 helper：成功、用户名密码错误、锁定用户、禁用用户。
- [ ] 覆盖 CSRF 提取 helper：页面缺少 token 时返回清晰错误。
- [ ] 覆盖创建 API token、吊销 token、删除已吊销 token helper。
- [ ] 覆盖创建节点、绑定凭据、触发探测、读取事件日志 helper。
- [ ] 覆盖创建应用、保存 Compose/env/scripts、上传版本包 helper。
- [ ] 覆盖读取发布版本、读取队列、读取任务详情 helper。
- [ ] 补一个端到端 smoke：登录 -> 创建应用 -> 生成 token -> OpenAPI 上传版本包 -> 查看发布版本和队列。

**Test scenarios:**
- Happy path：完整 smoke 在本地 router/test server 上跑通，不访问真实远程服务器。
- Error path：helper 找不到 CSRF 时错误消息包含页面路径。
- Error path：OpenAPI 上传失败时 helper 保留响应状态和 body 摘要。
- Integration：同一套 helper 能被后续真实浏览器 smoke 复用。

**Verification:**
- `cargo test -p e2e --lib -- --nocapture`
- `cargo test -p e2e --test smoke -- --nocapture`

---

### U9. 主机监控与格式化边界测试

**Goal:** 补齐总览页主机 CPU、内存、磁盘、磁盘速率、网络速率统计的计算边界，避免线上数据展示误导。

**Priority:** P2

**Requirements:** R1, R7

**Files:**
- Modify/Test: `api/src/host_metrics.rs`
- Optional Test: `api/src/web/mod.rs`

**Tasks:**
- [ ] 覆盖两位小数截断，不四舍五入。
- [ ] 覆盖 CPU、RAM、磁盘使用率的零值、满值、异常值。
- [ ] 覆盖磁盘读写速率从两次采样差值计算。
- [ ] 覆盖网络上下行速率从两次采样差值计算。
- [ ] 覆盖刷新间隔 `1s`、`3s`、`5s`、`10s` 的参数解析。
- [ ] 覆盖缺少磁盘能力数据时 UI 显示 unknown，而不是错误数值。

**Test scenarios:**
- Happy path：`12.349` 展示为 `12.34`。
- Edge case：前后采样时间间隔为 0 时不除零。
- Edge case：计数器回绕或减少时速率归零并记录异常。

**Verification:**
- `cargo test -p api host_metrics::tests:: -- --nocapture`

---

### U10. CLI 与 migration 规范测试

**Goal:** 保护本项目自研 SQL migration 规范和启动命令边界，减少正式部署前的人为失误。

**Priority:** P2

**Requirements:** R8

**Files:**
- Modify/Test: `api/src/migrations.rs`
- Modify/Test: `api/src/main.rs`
- Optional Test: `api/src/maintenance.rs`

**Tasks:**
- [ ] 覆盖 `migrate status` 在干净状态、未应用状态、checksum 不一致状态下的输出语义。
- [ ] 覆盖 `migrate create <snake_case_name>` 的合法和非法名称。
- [ ] 覆盖 `migrate guard <base>` 对历史 migration 修改的拒绝。
- [ ] 覆盖无效 CLI 参数返回帮助或明确错误。
- [ ] 覆盖 `clean-demo-data` dry-run 和非 dry-run 的边界。
- [ ] 覆盖 settings/env 解析端口、data_dir、database_url 的基本错误。

**Test scenarios:**
- Happy path：创建 migration 生成递增编号和 snake_case 文件名。
- Error path：尝试创建 `Bad Name` 返回非法名称错误。
- Error path：guard 检测历史 migration 修改时返回失败。
- Edge case：仓库没有 `origin/main` 时错误提示引导用户传入可用基线。

**Verification:**
- `cargo test -p api migrations::tests:: -- --nocapture`
- `cargo test -p api main::tests:: -- --nocapture`

---

### U11. 凭据文件与密钥管理边界测试

**Goal:** 补齐节点凭据生成、上传、禁用、删除和文件权限错误，避免 SSH 私钥状态和数据库状态不一致。

**Priority:** P2

**Requirements:** R3, R4

**Files:**
- Modify/Test: `api/src/node_credentials.rs`
- Optional Test: `api/src/web/mod.rs`

**Tasks:**
- [ ] 覆盖生成 RSA/ed25519 凭据时文件落盘和数据库记录一致。
- [ ] 覆盖上传私钥时权限、换行、文件名和 public key 派生。
- [ ] 覆盖禁用凭据后节点探测不能继续使用。
- [ ] 覆盖删除凭据时仍被节点引用的拒绝策略。
- [ ] 覆盖凭据文件丢失或目录不可写时的错误提示。
- [ ] 覆盖公钥复制内容不包含私钥。

**Test scenarios:**
- Happy path：生成凭据后 public key 可用于页面复制，private key 保存到 data_dir。
- Error path：凭据被节点绑定时删除失败，错误说明引用节点。
- Error path：文件系统写入失败时不留下半条数据库记录。

**Verification:**
- `cargo test -p api node_credentials::tests:: -- --nocapture`

---

## Recommended Execution Order

1. 先做 U1、U2、U3：覆盖发布主链路，收益最高，也最能推动 `api/src/apps.rs` 覆盖率。
2. 再做 U4、U5：覆盖已在线上暴露过风险的 SSH known_hosts 和节点探测问题。
3. 再做 U6、U7：保护 Web 后台安全边界和 OpenAPI AI 接入边界。
4. 再做 U8：补 E2E helper，使后续 smoke 更可信。
5. 最后做 U9、U10、U11：补监控展示、CLI/migration、凭据文件这些中等风险边界。

如果希望快速提升覆盖率并保持价值，建议第一轮只做 U1、U2、U4、U6、U7，完成后重新生成覆盖率报告再决定下一轮。

---

## System-Wide Impact

- **发布可靠性:** 新测试会覆盖 release/queue/snapshot/task 的状态一致性，降低串行发布、失败阻塞和回滚入口的回归风险。
- **运维可排障性:** SSH、节点探测、安装和事件日志测试会保护正式环境最常见的“连不上但不知道原因”问题。
- **安全边界:** RBAC、CSRF、token、凭据测试会保护后台控制面，避免部署平台被误操作或越权操作。
- **外部接入:** OpenAPI 测试会保证其他项目的 AI 或脚本只靠文档和接口就能上传版本包。
- **测试维护成本:** fake runner 和临时 SQLite 会避免真实外部依赖，保持本地和 CI 稳定。

---

## Risks & Mitigations

- 风险：补 Web HTML 断言容易因 UI 调整频繁失败。
  缓解：只断言关键语义、状态码、重定向、权限和数据状态。

- 风险：部署/SSH 测试过度 mock 后不能发现真实命令问题。
  缓解：fake runner 断言完整命令参数、环境变量、known_hosts、identity file 和错误输出。

- 风险：覆盖率目标诱导补低价值测试。
  缓解：每个测试必须关联一个任务单元和风险场景，低价值覆盖不纳入本计划。

- 风险：`api/src/apps.rs` 和 `api/src/web/mod.rs` 过大，新增测试难以定位。
  缓解：先补行为测试；如果测试 setup 过重，再做小范围 helper 抽取，不借机大重构。

- 风险：E2E 运行时间增加。
  缓解：把 helper 自测放在 `e2e --lib`，完整 smoke 保持少量关键流程。

---

## Verification Plan

每轮实现后优先执行：

```text
cargo fmt --all --check
cargo check --workspace
cargo test -p api -- --nocapture
cargo test -p e2e --lib -- --nocapture
```

涉及 E2E smoke 时执行：

```text
cargo test -p e2e --test smoke -- --nocapture
```

提交前或阶段收口时执行：

```text
cargo clippy --workspace --all-targets -- -D warnings
cargo llvm-cov --workspace --json --summary-only --output-path tmp/coverage-summary.json
```

验收目标：

- 总行覆盖率从 `80.01%` 提升到 `82%+`。
- `api/src/apps.rs` 覆盖率优先提升，且新增测试集中在 release/queue/snapshot/task 主链路。
- `api/src/deploy.rs` 和 `api/src/nodes.rs` 覆盖 known_hosts、探测错误和事件日志。
- `api/src/web/mod.rs` 覆盖关键写操作的认证、授权、CSRF 和 OpenAPI 边界。
- `e2e/src/lib.rs` 增加 helper 自测，后续 smoke 失败时错误信息更可定位。

---

## Completion Criteria

- [ ] U1-U3 至少完成两项，发布主链路测试覆盖明显提升。
- [ ] U4 或 U5 至少完成一项，SSH/节点探测风险得到保护。
- [ ] U6 或 U7 至少完成一项，控制面安全或外部投递边界得到保护。
- [ ] 覆盖率报告重新生成到 `tmp/coverage-summary.json`，并在最终总结中记录最新总覆盖率。
- [ ] 新增测试不依赖真实 Docker、SSH、systemd 或公网服务。
- [ ] `cargo fmt --all --check`、`cargo check --workspace`、相关 `cargo test` 通过。

---

## Sources & References

- Coverage baseline: `tmp/coverage-summary.json`
- Related plan: `docs/plans/2026-06-23-compose-only-release-pipeline.md`
- Related plan: `docs/plans/2026-07-03-001-feat-qfy-sc-template-alignment-plan.md`
- Main application service: `api/src/apps.rs`
- Web routes and OpenAPI: `api/src/web/mod.rs`
- Deployment command layer: `api/src/deploy.rs`
- Node probe layer: `api/src/nodes.rs`
- Task service: `api/src/tasks.rs`
- E2E helper: `e2e/src/lib.rs`
