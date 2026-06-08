# API SQL 迁移手册

## 适用范围

本手册适用于 easy-deploy 的 SQLite 主库迁移。当前项目只有一个业务库，迁移文件统一放在：

```text
api/migrations/
```

本项目不照搬 qfy-sc 的多库 worker 迁移模式，只保留轻量但必要的迁移纪律：可查看状态、可执行迁移、可创建新迁移、可保护历史迁移。

## 常用命令

在仓库根目录执行：

```bash
cargo run -p api -- migrate status
cargo run -p api -- migrate up
cargo run -p api -- migrate create add_deployment_index
cargo run -p api -- migrate guard
```

指定数据库：

```bash
cargo run -p api -- --database-url sqlite://easy-deploy.db migrate status
cargo run -p api -- --database-url sqlite://easy-deploy.db migrate up
```

指定 guard 对比分支或提交：

```bash
cargo run -p api -- migrate guard origin/main
cargo run -p api -- migrate guard HEAD
```

## 文件命名

当前历史迁移已经使用 `NNNN_name.sql` 风格，例如：

```text
0001_init.sql
0028_api_tokens.sql
```

后续继续沿用该风格，避免在同一个 SQLite 迁移序列中混入时间戳版本号。

新迁移文件必须满足：

```text
NNNN_snake_case.sql
```

要求：

- 版本号只递增，不复用旧编号。
- 名称使用小写英文、数字和下划线。
- 一次迁移只做一个主要目的。
- 历史 migration 禁止修改、删除、重命名。
- 结构问题用新的补丁 migration 修复，不改旧文件。

## 发布顺序

推荐发布前流程：

```text
备份 SQLite 数据库
-> cargo run -p api -- migrate status
-> cargo run -p api -- migrate guard
-> cargo run -p api -- migrate up
-> 启动或重启 api 服务
-> 执行 smoke / e2e 验收
```

当前服务启动时仍会自动执行 pending migration。这是为了保持单机部署工具简单易用。后续如果部署平台自身要做多实例高可用，再把启动自动迁移改成显式发布步骤。

## 数据修复边界

适合放进 migration：

- 新表、新字段、新索引。
- 小范围、可重复、确定性的结构配套数据修正。
- 内置权限、基础配置等必须和 schema 同步变化的数据。

不适合放进常规 migration：

- 大批量 backfill。
- 需要长时间锁表或大量 I/O 的数据修复。
- 依赖外部服务或部署环境状态的修复。

这类任务后续应做成单独的维护命令或后台任务，并在执行前明确备份和回滚方案。

## Guard 规则

`migrate guard` 会检查 `api/migrations/` 下的变更：

- 新增 SQL 文件必须匹配 `NNNN_snake_case.sql`。
- 历史 SQL 文件不能修改。
- 历史 SQL 文件不能删除。
- 历史 SQL 文件不能重命名。
- 迁移目录内不允许放非 SQL 文件。

默认对比 `origin/main`、`main`、`master` 或 `HEAD` 中可用的第一个 ref；也可以显式传入 base ref。
