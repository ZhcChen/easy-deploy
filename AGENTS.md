# easy-deploy 项目协作规则

## 语言规范
- 所有文档、讨论、任务总结与提交说明默认使用简体中文。
- 框架、库、协议、命令、配置键等固定术语可保留英文。

## 项目定位
- 本项目是一个使用 Rust 开发的简单易用部署平台。
- 主模块为 `api`，负责 Web 后台、认证授权、部署编排与页面渲染。
- `e2e` 模块用于验收测试和端到端 smoke。
- 当前后台服务默认端口为 `9066`。
- UI 优先使用 HTML + CSS + htmx 思路，尽量保持低依赖；除非用户明确要求，不引入重型前端框架。

## CE 工作流
- 本项目默认启用 Compound Engineering 作为主要 AI 工作架构。
- 需求不清或范围未定时，优先先澄清或产出 `docs/brainstorms/`。
- 需求清晰但涉及跨模块设计时，优先产出或续写 `docs/plans/`。
- 进入执行阶段后，按计划实现、验证、复盘。
- 解决方案或可复用经验沉淀到 `docs/solutions/`。
- 已有计划文档时优先复用和续写，不要重复生成平行文档。

## 子代理与模型约束
- 未经用户明确要求，不使用 subagent / 子代理；即使用户允许使用，也必须遵守本节模型约束。
- subagent / 子代理严禁使用 GPT 5.6 及任何 GPT 5.6 相关、派生、别名、预览或实验模型。
- 如果工具、插件或平台默认会为 subagent / 子代理选择 GPT 5.6 相关模型，必须拒绝该配置，改用允许模型或回到主线程顺序执行。
- 本约束优先级高于 CE、插件、工具或外部文档中的默认建议。

## 目录与产物约定
- 需求/产品定义：`docs/brainstorms/`
- 技术计划：`docs/plans/`
- 解决方案/经验沉淀：`docs/solutions/`
- CE 运行期中间产物：`.context/compound-engineering/`
- 临时截图、浏览器调试产物、一次性导出预览、排障草稿等非正式交付物统一放入 `tmp/` 或其子目录；不要散放在仓库根目录。`tmp/` 必须保持在 `.gitignore` 中。
- 所有文档路径引用使用仓库相对路径，不使用绝对路径。

## 开发与验证
- Rust 代码提交前优先运行与改动范围匹配的验证：
  - 格式检查：`cargo fmt --all --check`
  - 编译检查：`cargo check --workspace`
  - 后端单测：`cargo test -p api`
  - E2E smoke：`cargo test -p e2e --test smoke -- --nocapture`
  - 风险较高或提交前收口时：`cargo clippy --workspace --all-targets -- -D warnings`
- 如果因环境、外部依赖或耗时原因无法完成完整验证，必须在最终回复中说明边界和已完成的验证。
- 不提交明显编译失败、测试失败或半成品状态，除非用户明确要求保存现场；这种提交必须在 commit message 中标记 `WIP` 或阻塞点。

## 正式环境部署约定
- 当前 easy-deploy 的正式环境为服务器 SSH 别名 `qfy-sc-test`。
- 正式访问域名为 `https://easy-deploy.quanxinfu.com`，由服务器现有 Caddy 托管 HTTPS 和反向代理。
- 后续用户说“部署正式环境”“发正式”“部署生产”等同类指令时，默认目标就是 `qfy-sc-test` 上的 easy-deploy 正式环境，除非用户当次明确指定其他服务器或域名。
- 正式环境服务使用 systemd 单机部署：
  - systemd 服务：`easy-deploy.service`
  - 运行用户：`root:root`
  - 后端监听：`127.0.0.1:9066`
  - 程序文件：`/opt/easy-deploy/easy-deploy-api`
  - 环境配置：`/etc/easy-deploy/easy-deploy.env`
  - SQLite 数据库：`/var/lib/easy-deploy/easy-deploy.db`
  - Caddy 配置：`/etc/caddy/Caddyfile.d/easy-deploy.quanxinfu.com.caddy`
- 部署正式环境时，优先在本地或构建容器中产出 Linux x86_64 二进制，再上传到服务器执行 `scripts/deploy-systemd.sh`；不要在 `qfy-sc-test` 上直接 release 编译，服务器内存较小，容易拖垮 SSH 和系统负载。
- 修改 Caddy 时只新增或调整 `easy-deploy.quanxinfu.com.caddy` 这一份独立配置，必须先执行 `caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile`，通过后再 `systemctl reload caddy`，不得影响 `/etc/caddy/Caddyfile.d/` 下其他项目配置。
- 正式部署完成后至少验证：
  - `systemctl is-active easy-deploy`
  - `curl http://127.0.0.1:9066/healthz`
  - `curl https://easy-deploy.quanxinfu.com/healthz`
  - 必要时查看 `journalctl -u easy-deploy -n 80 --no-pager`

## SQL 迁移规范
- SQLite 主库 migration 统一放在 `api/migrations/`。
- 当前历史迁移使用 `NNNN_name.sql` 风格，后续继续追加递增编号，不改成时间戳格式。
- 新迁移通过 `cargo run -p api -- migrate create <snake_case_name>` 创建。
- 发布或提交前优先执行 `cargo run -p api -- migrate status` 和 `cargo run -p api -- migrate guard origin/main`；若当前仓库没有 `origin/main`，再使用可用的基线分支或提交。
- 历史 migration 禁止修改、删除、重命名；结构问题用新的补丁 migration 修复。
- `migrate guard` 是基于 Git diff 的历史迁移保护；`migrate status` 中的 sqlx checksum 状态用于识别已应用迁移是否被改动，二者都要保留，不能互相替代。
- 大批量 backfill、外部依赖修复、长时间数据修复不放进常规 migration，后续做成独立维护命令或任务。
- 详细规则见 `docs/runbooks/api-migrations.md`。

## Git 提交规范
- 提交说明默认使用简体中文，优先概括业务目的和改动范围。
- 用户明确要求实现、修复、调整、完善、联调、继续任务或整理提交时，视为授权在任务达到稳定可验收状态后自行提交并推送；不要每次提交前反复询问用户。
- 后续默认直接在 `main` 分支开发、提交与推送；除非用户明确要求隔离风险或创建功能分支，不再额外使用长期功能分支。
- 默认采用类似 qfy-sc 的 Conventional Commit 风格：
  - `feat:` 新能力或用户可见能力
  - `fix:` 修复缺陷、断言失败、行为回归
  - `docs:` 文档、计划、规则
  - `test:` 测试或验收覆盖
  - `chore:` 工程维护、依赖、配置
  - `refactor:` 不改变行为的结构调整
- 示例：
  - `feat: 初始化部署平台基础能力`
  - `fix: 修复 e2e 会话与审计断言`
  - `docs: 同步项目协作与提交规则`
- 一次任务包含多个相对独立功能点时，按功能边界、风险边界或可验证阶段拆分多个 commit；每个 commit 应保持语义清晰、可独立说明，避免把无关改动混在一起。
- 每个 commit 完成后立即 `git push` 到当前分支，降低本地机器故障导致代码丢失的风险；不要积压多个已完成 commit 长时间不推送。
- 提交前必须查看 `git status` 和待提交 diff，只提交本次任务相关文件。
- 提交前应完成与改动范围匹配的测试、构建、格式检查或页面验证；如果因环境、外部凭证、真实三方接口等原因无法验证，必须在最终回复中说明边界。
- 工作区存在用户或其他任务留下的无关改动时，保留不动，不得顺手混入。
- 不提交明显编译失败、测试失败或半成品状态，除非用户明确要求保存现场；这种情况下 commit message 必须标明 `WIP` / 阻塞点，并仍需及时 push。

## 浏览器与 E2E
- 浏览器是较重工具；源码、日志、HTTP 请求或现有测试足以完成任务时，不额外打开浏览器。
- 普通页面操作、截图、冒烟验证优先使用 `agent-browser`。
- DOM、Network、Console、Performance 深度排查优先使用 Chrome DevTools MCP。
- 正式 E2E / 回归脚本优先使用项目内测试。
- 同一任务默认复用已有浏览器 session/page，任务结束后清理临时页面或 session。

## 终端输出格式
- 默认不要使用 Markdown 表格。
- 需要展示表格数据时，优先使用适合终端阅读的 Unicode 线框表格。
- 小型表格也可以使用对齐良好的纯文本列；除非用户明确要求 Markdown，否则避免 Markdown table。

## Context7 使用准则
- 需要官方库或框架资料时，优先使用 Context7 MCP 服务。
- 调用文档查询前先解析准确的 `/org/project[/version]` 标识；用户已提供完整 ID 时可直接使用。
- 名称歧义时说明筛选理由；不确定需求时先确认。
- 使用 Context7 文档撰写答案时，确保内容与原文一致并注明来源。

## Chrome DevTools MCP 使用准则
- 需排查浏览器端行为、排版或网络问题时，优先调用 Chrome DevTools MCP。
- 调试前明确目标页面和期望采集的数据，如 DOM、Network、Console。
- 获取结果后整理关键观察，避免遗漏上下文。

## 执行优先级
1. 用户明确指令
2. 当前项目根目录下的规范文件
3. CE 工作流约定
4. 全局默认行为

<!-- BEGIN COMPOUND CODEX TOOL MAP -->
## Compound Codex Tool Mapping (Claude Compatibility)

This section maps Claude Code plugin tool references to Codex behavior.
Only this block is managed automatically.

Tool mapping:
- Read: use shell reads (cat/sed) or rg
- Write: create files via shell redirection or apply_patch
- Edit/MultiEdit: use apply_patch
- Bash: use shell_command
- Grep: use rg (fallback: grep)
- Glob: use rg --files or find
- LS: use ls via shell_command
- WebFetch/WebSearch: use curl or Context7 for library docs
- AskUserQuestion/Question: present choices as a numbered list in chat and wait for a reply number. For multi-select (multiSelect: true), accept comma-separated numbers. Never skip or auto-configure — always wait for the user's response before proceeding.
- Task (subagent dispatch) / Subagent / Parallel: run sequentially in main thread; use multi_tool_use.parallel for tool calls
- TaskCreate/TaskUpdate/TaskList/TaskGet/TaskStop/TaskOutput (Claude Code task-tracking, current): use update_plan (Codex's task-tracking primitive)
- TodoWrite/TodoRead (Claude Code task-tracking, legacy — deprecated, replaced by Task* tools): use update_plan
- Skill: open the referenced SKILL.md and follow it
- ExitPlanMode: ignore
<!-- END COMPOUND CODEX TOOL MAP -->
