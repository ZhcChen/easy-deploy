# systemd 单机部署手册

## 部署定位

easy-deploy 控制台自身按单机部署设计。推荐用 systemd 直接托管 `api` 二进制，减少运行时依赖。

平台可以管理多节点部署目标，但控制台服务本身只运行一个实例，不做多副本、不做滚动发布。更新 easy-deploy 自身时使用：

```text
停止单个 systemd service -> 备份旧二进制和 SQLite -> 替换二进制 -> 启动新版本
```

服务启动时会自动执行 pending SQL migration，因此同一时间只能有一个 `api` 实例连接同一个 SQLite 数据库。

## 推荐 Linux 目录

```text
/opt/easy-deploy/
├── easy-deploy-api              # 可执行文件
└── apps/                        # 默认本机节点应用部署目录

/etc/easy-deploy/
└── easy-deploy.env              # systemd EnvironmentFile

/var/lib/easy-deploy/
├── easy-deploy.db               # SQLite 主库
├── data/                        # EASY_DEPLOY_DATA_DIR
└── backups/                     # 更新前备份

/etc/systemd/system/
└── easy-deploy.service
```

这套目录遵循常见 Linux 约定：

- `/opt/easy-deploy/easy-deploy-api` 放控制台程序。
- `/opt/easy-deploy/apps` 放默认本机节点应用部署目录。
- `/etc/easy-deploy` 放配置。
- `/var/lib/easy-deploy` 放需要持久化的数据。
- 日志交给 `journald`，不额外维护日志文件。

## 首次部署

部署脚本路径：

```text
scripts/deploy-systemd.sh
```

在构建机或目标机准备 release 二进制：

```bash
cargo build -p api --release
```

在目标 Linux 服务器上执行：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build
```

如果服务器本身安装了 Rust toolchain，也可以直接在仓库内构建并部署：

```bash
sudo bash scripts/deploy-systemd.sh
```

部署脚本会执行：

- 检查 Linux + systemd 环境。
- 判断 `/etc/systemd/system/easy-deploy.service` 是否存在，用于区分首次部署和更新部署。
- 使用 `root:root` 作为默认 systemd 运行用户和组。
- 创建 `/opt/easy-deploy`、`/etc/easy-deploy`、`/var/lib/easy-deploy`。
- 创建 `/opt/easy-deploy/apps` 作为默认本机节点应用部署目录。
- 安装二进制到 `/opt/easy-deploy/easy-deploy-api`。
- 生成 `/etc/easy-deploy/easy-deploy.env`。
- 生成 `/etc/systemd/system/easy-deploy.service`。
- `systemctl daemon-reload`。
- `systemctl enable easy-deploy.service`。
- 启动服务。

默认监听：

```text
127.0.0.1:9066
```

如果需要让外部直接访问，可以改为：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build --bind 0.0.0.0:9066
```

生产环境更推荐绑定本机地址，再由 Nginx/Caddy 做 HTTPS 反向代理。

## 运行权限

脚本默认使用 root 运行：

```text
root:root
```

原因是 easy-deploy 是部署控制台，不是普通业务 Web 服务。它需要管理本机 Docker Compose、读取主机进程 IO、写入部署目录并执行发布脚本；如果使用低权限用户，磁盘 IO 进程 Top、Docker 操作和部分本机部署动作会被 Linux 权限拦截。

如果确实只通过 SSH 管理远程节点，并且不需要本机 Docker、进程 IO 和本机部署能力，可以显式降权运行：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build --user easy-deploy --group easy-deploy
```

降权运行前需要自行创建用户和组，或让脚本创建 `easy-deploy` 系统用户。降权后如需访问 Docker，需要额外配置 docker 组、sudoers 或 polkit；脚本不会自动写这些宿主机权限规则。

正式环境 `easy-deploy.quanxinfu.com` 当前按 `root:root` 运行。

## 更新部署

构建新二进制后重复执行同一条部署命令：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build
```

脚本会判断 service 是否已存在：

- 已存在并运行：停止 service，备份旧二进制和 SQLite 数据库，替换二进制，启动 service。
- 已存在但未运行：备份旧二进制和 SQLite 数据库，替换二进制，启动 service。
- 不存在：按首次部署创建 service 并启动。

每次更新前会备份到：

```text
/var/lib/easy-deploy/backups/<yyyyMMddHHmmss>/
```

包含：

```text
easy-deploy-api
easy-deploy.db
```

脚本默认只保留最近 5 份自部署备份。清理动作发生在新 service 启动并通过 `systemctl status` 检查之后；如果使用 `--no-start`，脚本不会自动清理旧备份，避免在未验证新版本可启动时丢失回滚点。这里的启动成功仅代表 systemd 启动检查通过，正式发布完成后仍需按验收要求访问 `/healthz`。

如需调整保留数量：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build --backups-to-keep 10
```

`--backups-to-keep` 必须是 1 到 1000 之间的整数。备份目录中非 14 位数字时间戳格式的目录不会被脚本自动删除；如需长期保留某次备份，可以先移动到其他归档目录。

部署后可以查看实际保留的备份：

```bash
ls -1 /var/lib/easy-deploy/backups/ | sort
```

## 配置

脚本生成的配置文件：

```bash
sudo vim /etc/easy-deploy/easy-deploy.env
```

首次部署会生成该文件。后续更新时，如果该文件已经存在，脚本默认保留现有配置，避免覆盖线上调整过的监听地址、Cookie 策略或日志级别。需要强制按参数重新生成时，显式加：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build --force-env
```

默认内容类似：

```bash
EASY_DEPLOY_BIND=127.0.0.1:9066
EASY_DEPLOY_DATABASE_URL=sqlite:///var/lib/easy-deploy/easy-deploy.db
EASY_DEPLOY_DATA_DIR=/var/lib/easy-deploy/data
EASY_DEPLOY_COOKIE_SECURE=false
EASY_DEPLOY_COMMAND_TIMEOUT_SECS=120
EASY_DEPLOY_UPLOADED_BINARY_RELEASES_TO_KEEP=4
RUST_LOG=api=info,tower_http=info,info
```

修改配置后执行：

```bash
sudo systemctl restart easy-deploy
```

## 常用命令

查看状态：

```bash
sudo systemctl status easy-deploy
```

查看日志：

```bash
journalctl -u easy-deploy -f
```

重启：

```bash
sudo systemctl restart easy-deploy
```

停止：

```bash
sudo systemctl stop easy-deploy
```

开机自启：

```bash
sudo systemctl enable easy-deploy
```

## dry-run

先查看脚本会执行哪些操作：

```bash
sudo bash scripts/deploy-systemd.sh --binary ./target/release/api --skip-build --dry-run
```

`--dry-run` 不要求当前机器是 Linux/systemd，也不会写入文件或启动服务，适合在本机预览目录、service 文件和 env 文件内容。真实服务状态会在目标 Linux 服务器执行时重新判断。

## 常用脚本参数

```text
--binary <path>             使用已构建好的 api 二进制。
--skip-build                跳过 cargo build，通常和 --binary 一起使用。
--bind <addr:port>          修改监听地址，默认 127.0.0.1:9066。
--user <name> --group <g>   修改 systemd 运行用户和组，默认 root:root。
--backups-to-keep <n>       自部署备份保留数量，默认 5，范围 1..1000。
--no-start                  只安装/更新文件，不启动服务；如果原服务正在运行，更新前会先停止。
--force-env                 覆盖已有 /etc/easy-deploy/easy-deploy.env。
--dry-run                   只打印将执行的动作。
```

## 回滚

找到上一次备份：

```bash
ls -lah /var/lib/easy-deploy/backups/
```

停止服务：

```bash
sudo systemctl stop easy-deploy
```

恢复二进制：

```bash
sudo install -m 0755 -o root -g root \
  /var/lib/easy-deploy/backups/<backup>/easy-deploy-api \
  /opt/easy-deploy/easy-deploy-api
```

如需恢复数据库：

```bash
sudo install -m 0640 -o root -g root \
  /var/lib/easy-deploy/backups/<backup>/easy-deploy.db \
  /var/lib/easy-deploy/easy-deploy.db
```

启动服务：

```bash
sudo systemctl start easy-deploy
```

注意：如果新版本已经执行了不可逆 schema 迁移，只恢复旧二进制可能不够，需要同时恢复对应备份数据库。

## 卸载

停止并禁用服务：

```bash
sudo systemctl disable --now easy-deploy
sudo rm -f /etc/systemd/system/easy-deploy.service
sudo systemctl daemon-reload
```

删除程序和配置：

```bash
sudo rm -rf /opt/easy-deploy
sudo rm -rf /etc/easy-deploy
```

如确认不需要保留数据，再删除：

```bash
sudo rm -rf /var/lib/easy-deploy
```
