#!/usr/bin/env bash
set -euo pipefail

SERVICE_NAME="easy-deploy"
INSTALL_DIR="/opt/easy-deploy"
CONFIG_DIR="/etc/easy-deploy"
DATA_DIR="/var/lib/easy-deploy"
LOCAL_APP_DIR="/opt/easy-deploy/apps"
RUN_USER="root"
RUN_GROUP="root"
BIND_ADDR="127.0.0.1:9066"
COOKIE_SECURE="false"
COMMAND_TIMEOUT_SECS="120"
RELEASES_TO_KEEP="4"
BACKUPS_TO_KEEP="5"
BACKUPS_TO_KEEP_MAX="1000"
BINARY_PATH=""
SKIP_START="false"
SKIP_BUILD="false"
DRY_RUN="false"
FORCE_ENV="false"
SERVICE_EXISTS="false"
SERVICE_ACTIVE="false"

usage() {
  cat <<'EOF'
Usage:
  sudo bash scripts/deploy-systemd.sh [options]

Options:
  --binary <path>              Use an existing easy-deploy-api binary.
  --skip-build                 Do not run cargo build; requires --binary.
  --service-name <name>        systemd service name. Default: easy-deploy.
  --install-dir <path>         Program directory. Default: /opt/easy-deploy.
  --config-dir <path>          Config directory. Default: /etc/easy-deploy.
  --data-dir <path>            Data directory. Default: /var/lib/easy-deploy.
  --local-app-dir <path>       Writable local app deployment directory. Default: /opt/easy-deploy/apps.
  --user <name>                Runtime user. Default: root.
  --group <name>               Runtime group. Default: root.
  --bind <addr:port>           Listen address. Default: 127.0.0.1:9066.
  --cookie-secure <true|false> Secure cookie flag. Default: false.
  --command-timeout-secs <n>   Command timeout. Default: 120.
  --releases-to-keep <n>       Uploaded binary releases to keep. Default: 4.
  --backups-to-keep <n>        Self-deployment backups to keep. Default: 5, max: 1000.
  --no-start                   Install/update files but do not start or restart service.
  --force-env                  Rewrite existing environment file.
  --dry-run                    Print actions without changing the system.
  -h, --help                   Show help.

Recommended Linux layout:
  /opt/easy-deploy/easy-deploy-api      executable
  /opt/easy-deploy/apps                 local app deployment work dir
  /etc/easy-deploy/easy-deploy.env      environment config
  /var/lib/easy-deploy/easy-deploy.db   SQLite database
  /var/lib/easy-deploy/data             runtime data directory
  /var/lib/easy-deploy/backups          deployment backups
  /etc/systemd/system/easy-deploy.service
EOF
}

log() {
  printf '[easy-deploy] %s\n' "$*"
}

die() {
  printf '[easy-deploy] error: %s\n' "$*" >&2
  exit 1
}

need_value() {
  local name="$1"
  local value="${2:-}"
  if [[ -z "$value" || "$value" == --* ]]; then
    die "$name requires a value"
  fi
  printf '%s' "$value"
}

require_positive_integer_at_most() {
  local name="$1"
  local value="$2"
  local max="$3"
  if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
    die "$name must be a positive integer"
  fi
  if (( ${#value} > ${#max} )) || (( 10#$value > 10#$max )); then
    die "$name must be less than or equal to $max"
  fi
}

run() {
  if [[ "$DRY_RUN" == "true" ]]; then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

service_file_path() {
  printf '/etc/systemd/system/%s.service' "$SERVICE_NAME"
}

write_file() {
  local path="$1"
  local mode="$2"
  local owner="$3"
  local content="$4"
  if [[ "$DRY_RUN" == "true" ]]; then
    log "write $path"
    printf '%s\n' "$content"
    return
  fi
  local tmp
  tmp="$(mktemp)"
  printf '%s\n' "$content" > "$tmp"
  install -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$tmp" "$path"
  rm -f "$tmp"
}

service_unit_exists() {
  local service_file
  service_file="$(service_file_path)"
  [[ -f "$service_file" ]] && return 0
  systemctl list-unit-files "$SERVICE_NAME.service" --no-legend --no-pager 2>/dev/null | grep -q "^$SERVICE_NAME\\.service"
}

require_linux_systemd() {
  if [[ "$DRY_RUN" == "true" ]]; then
    log "dry-run: skip Linux/systemd environment check"
    return
  fi
  [[ "$(uname -s)" == "Linux" ]] || die "this script only supports Linux"
  command -v systemctl >/dev/null 2>&1 || die "systemctl not found"
  if [[ ! -d /run/systemd/system ]]; then
    die "systemd does not seem to be PID 1 on this server"
  fi
}

require_root() {
  if [[ "$DRY_RUN" == "true" ]]; then
    log "dry-run: skip root check"
    return
  fi
  if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    die "please run as root, for example: sudo bash scripts/deploy-systemd.sh"
  fi
}

ensure_runtime_account() {
  if [[ "$DRY_RUN" == "true" ]]; then
    log "dry-run: ensure system user/group $RUN_USER:$RUN_GROUP"
    return
  fi
  if ! getent group "$RUN_GROUP" >/dev/null 2>&1; then
    run groupadd --system "$RUN_GROUP"
  fi
  if ! id -u "$RUN_USER" >/dev/null 2>&1; then
    run useradd --system --gid "$RUN_GROUP" --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$RUN_USER"
  fi
}

build_binary_if_needed() {
  if [[ "$SKIP_BUILD" == "true" ]]; then
    [[ -n "$BINARY_PATH" ]] || die "--skip-build requires --binary"
    return
  fi
  command -v cargo >/dev/null 2>&1 || die "cargo not found; use --binary <path> on production servers without Rust"
  log "building api release binary"
  run cargo build -p api --release
  if [[ -z "$BINARY_PATH" ]]; then
    BINARY_PATH="target/release/api"
  fi
}

validate_binary() {
  [[ -n "$BINARY_PATH" ]] || die "missing binary path"
  [[ -f "$BINARY_PATH" ]] || die "binary not found: $BINARY_PATH"
}

detect_deploy_context() {
  local service_file
  service_file="$(service_file_path)"
  local current_binary="$INSTALL_DIR/easy-deploy-api"
  local db_path="$DATA_DIR/easy-deploy.db"

  if [[ "$DRY_RUN" == "true" ]]; then
    if [[ -f "$service_file" ]]; then
      SERVICE_EXISTS="true"
      if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "$SERVICE_NAME.service" 2>/dev/null; then
        SERVICE_ACTIVE="true"
        log "dry-run: detected existing running service $SERVICE_NAME.service"
      else
        SERVICE_ACTIVE="unknown"
        log "dry-run: detected existing service file $service_file; active state will be checked on the server"
      fi
    elif [[ -f "$current_binary" || -f "$db_path" ]]; then
      SERVICE_EXISTS="false"
      log "dry-run: no service file found, but existing binary/database may be reused"
    else
      SERVICE_EXISTS="false"
      log "dry-run: first install, no service file found"
    fi
    return
  fi

  if service_unit_exists; then
    SERVICE_EXISTS="true"
    if systemctl is-active --quiet "$SERVICE_NAME.service"; then
      SERVICE_ACTIVE="true"
      log "update mode: existing running service $SERVICE_NAME.service"
    else
      SERVICE_ACTIVE="false"
      log "update mode: existing stopped service $SERVICE_NAME.service"
    fi
  elif [[ -f "$current_binary" || -f "$db_path" ]]; then
    SERVICE_EXISTS="false"
    SERVICE_ACTIVE="false"
    log "install mode: no service unit found, existing binary/database will be preserved and backed up"
  else
    SERVICE_EXISTS="false"
    SERVICE_ACTIVE="false"
    log "install mode: first deployment"
  fi
}

stop_running_service_for_update() {
  if [[ "$SERVICE_ACTIVE" == "true" ]]; then
    log "stopping $SERVICE_NAME.service before backup and binary replacement"
    run systemctl stop "$SERVICE_NAME.service"
  elif [[ "$SERVICE_ACTIVE" == "unknown" ]]; then
    log "dry-run: active state is unknown; real run stops the service first when it is active"
  fi
}

backup_existing_files() {
  local backup_dir="$DATA_DIR/backups/$(date +%Y%m%d%H%M%S)"
  local current_binary="$INSTALL_DIR/easy-deploy-api"
  local db_path="$DATA_DIR/easy-deploy.db"
  if [[ "$DRY_RUN" == "true" ]]; then
    log "backup existing files into $backup_dir when present"
    return
  fi
  if [[ ! -f "$current_binary" && ! -f "$db_path" ]]; then
    log "no existing binary or database to back up"
    return
  fi
  mkdir -p "$backup_dir"
  if [[ -f "$current_binary" ]]; then
    cp -a "$current_binary" "$backup_dir/easy-deploy-api"
  fi
  if [[ -f "$db_path" ]]; then
    cp -a "$db_path" "$backup_dir/easy-deploy.db"
  fi
  chown -R "$RUN_USER:$RUN_GROUP" "$DATA_DIR/backups"
}

prune_old_backups() {
  local backups_dir="$DATA_DIR/backups"
  if [[ "$SKIP_START" == "true" ]]; then
    log "skip backup pruning because --no-start was used"
    return
  fi
  if [[ ! -d "$backups_dir" ]]; then
    log "no deployment backup directory to prune: $backups_dir"
    return
  fi

  local backup_names=()
  local name
  local backup_path
  for backup_path in "$backups_dir"/*; do
    [[ -d "$backup_path" ]] || continue
    name="${backup_path##*/}"
    if [[ "$name" =~ ^[0-9]{14}$ ]]; then
      backup_names+=("$name")
    fi
  done

  if ((${#backup_names[@]} > 0)); then
    local sorted_output
    sorted_output="$(printf '%s\n' "${backup_names[@]}" | sort)" || die "failed to sort deployment backup list"
    local sorted_names=()
    while IFS= read -r name; do
      sorted_names+=("$name")
    done <<< "$sorted_output"
    backup_names=("${sorted_names[@]}")
  fi

  local count="${#backup_names[@]}"
  local prune_count=$((count - BACKUPS_TO_KEEP))
  if (( prune_count <= 0 )); then
    log "keeping $count deployment backups; prune threshold is $BACKUPS_TO_KEEP"
    return
  fi

  local index
  for ((index = 0; index < prune_count && index < count; index++)); do
    run rm -rf -- "$backups_dir/${backup_names[$index]}"
  done
  log "pruned $prune_count old deployment backups; kept newest $BACKUPS_TO_KEEP"
}

install_layout() {
  run install -d -m 0755 -o root -g root "$INSTALL_DIR"
  run install -d -m 0750 -o "$RUN_USER" -g "$RUN_GROUP" "$LOCAL_APP_DIR"
  run install -d -m 0750 -o root -g "$RUN_GROUP" "$CONFIG_DIR"
  run install -d -m 0750 -o "$RUN_USER" -g "$RUN_GROUP" "$DATA_DIR"
  run install -d -m 0750 -o "$RUN_USER" -g "$RUN_GROUP" "$DATA_DIR/data"
  run install -d -m 0750 -o "$RUN_USER" -g "$RUN_GROUP" "$DATA_DIR/backups"
}

install_binary() {
  run install -m 0755 -o root -g root "$BINARY_PATH" "$INSTALL_DIR/easy-deploy-api"
}

render_env_file() {
  cat <<EOF
EASY_DEPLOY_BIND=$BIND_ADDR
EASY_DEPLOY_DATABASE_URL=sqlite://$DATA_DIR/easy-deploy.db
EASY_DEPLOY_DATA_DIR=$DATA_DIR/data
EASY_DEPLOY_COOKIE_SECURE=$COOKIE_SECURE
EASY_DEPLOY_COMMAND_TIMEOUT_SECS=$COMMAND_TIMEOUT_SECS
EASY_DEPLOY_UPLOADED_BINARY_RELEASES_TO_KEEP=$RELEASES_TO_KEEP
RUST_LOG=api=info,tower_http=info,info
EOF
}

render_service_file() {
  cat <<EOF
[Unit]
Description=Easy Deploy API
Documentation=file://$CONFIG_DIR/easy-deploy.env
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$RUN_USER
Group=$RUN_GROUP
EnvironmentFile=$CONFIG_DIR/easy-deploy.env
ExecStart=$INSTALL_DIR/easy-deploy-api
WorkingDirectory=$DATA_DIR
Restart=on-failure
RestartSec=5
TimeoutStopSec=30
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=$DATA_DIR
ReadWritePaths=$LOCAL_APP_DIR

[Install]
WantedBy=multi-user.target
EOF
}

install_config() {
  local env_file="$CONFIG_DIR/easy-deploy.env"
  if [[ -f "$env_file" && "$FORCE_ENV" != "true" ]]; then
    log "preserving existing env file: $env_file (use --force-env to rewrite)"
    run chown "root:$RUN_GROUP" "$env_file"
    run chmod 0640 "$env_file"
  else
    write_file "$env_file" "0640" "root:$RUN_GROUP" "$(render_env_file)"
  fi
  write_file "$(service_file_path)" "0644" "root:root" "$(render_service_file)"
}

restart_service() {
  run systemctl daemon-reload
  run systemctl enable "$SERVICE_NAME.service"
  if [[ "$SKIP_START" == "true" ]]; then
    log "installed $SERVICE_NAME.service without starting it"
    return
  fi
  if [[ "$SERVICE_ACTIVE" == "true" ]]; then
    log "starting updated $SERVICE_NAME.service"
    run systemctl start "$SERVICE_NAME.service"
  elif [[ "$SERVICE_EXISTS" == "true" ]]; then
    log "starting existing $SERVICE_NAME.service"
    run systemctl start "$SERVICE_NAME.service"
  else
    log "starting new $SERVICE_NAME.service"
    run systemctl start "$SERVICE_NAME.service"
  fi
  run systemctl --no-pager --full status "$SERVICE_NAME.service"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      BINARY_PATH="$(need_value --binary "${2:-}")"
      shift 2
      ;;
    --binary=*)
      BINARY_PATH="${1#*=}"
      shift
      ;;
    --skip-build)
      SKIP_BUILD="true"
      shift
      ;;
    --service-name)
      SERVICE_NAME="$(need_value --service-name "${2:-}")"
      shift 2
      ;;
    --service-name=*)
      SERVICE_NAME="${1#*=}"
      shift
      ;;
    --install-dir)
      INSTALL_DIR="$(need_value --install-dir "${2:-}")"
      shift 2
      ;;
    --install-dir=*)
      INSTALL_DIR="${1#*=}"
      shift
      ;;
    --config-dir)
      CONFIG_DIR="$(need_value --config-dir "${2:-}")"
      shift 2
      ;;
    --config-dir=*)
      CONFIG_DIR="${1#*=}"
      shift
      ;;
    --data-dir)
      DATA_DIR="$(need_value --data-dir "${2:-}")"
      shift 2
      ;;
    --data-dir=*)
      DATA_DIR="${1#*=}"
      shift
      ;;
    --local-app-dir)
      LOCAL_APP_DIR="$(need_value --local-app-dir "${2:-}")"
      shift 2
      ;;
    --local-app-dir=*)
      LOCAL_APP_DIR="${1#*=}"
      shift
      ;;
    --user)
      RUN_USER="$(need_value --user "${2:-}")"
      shift 2
      ;;
    --user=*)
      RUN_USER="${1#*=}"
      shift
      ;;
    --group)
      RUN_GROUP="$(need_value --group "${2:-}")"
      shift 2
      ;;
    --group=*)
      RUN_GROUP="${1#*=}"
      shift
      ;;
    --bind)
      BIND_ADDR="$(need_value --bind "${2:-}")"
      shift 2
      ;;
    --bind=*)
      BIND_ADDR="${1#*=}"
      shift
      ;;
    --cookie-secure)
      COOKIE_SECURE="$(need_value --cookie-secure "${2:-}")"
      shift 2
      ;;
    --cookie-secure=*)
      COOKIE_SECURE="${1#*=}"
      shift
      ;;
    --command-timeout-secs)
      COMMAND_TIMEOUT_SECS="$(need_value --command-timeout-secs "${2:-}")"
      shift 2
      ;;
    --command-timeout-secs=*)
      COMMAND_TIMEOUT_SECS="${1#*=}"
      shift
      ;;
    --releases-to-keep)
      RELEASES_TO_KEEP="$(need_value --releases-to-keep "${2:-}")"
      shift 2
      ;;
    --releases-to-keep=*)
      RELEASES_TO_KEEP="${1#*=}"
      shift
      ;;
    --backups-to-keep)
      BACKUPS_TO_KEEP="$(need_value --backups-to-keep "${2:-}")"
      shift 2
      ;;
    --backups-to-keep=*)
      BACKUPS_TO_KEEP="${1#*=}"
      shift
      ;;
    --no-start)
      SKIP_START="true"
      shift
      ;;
    --force-env)
      FORCE_ENV="true"
      shift
      ;;
    --dry-run)
      DRY_RUN="true"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

case "$COOKIE_SECURE" in
  true|false) ;;
  *) die "--cookie-secure must be true or false" ;;
esac

[[ "$SERVICE_NAME" =~ ^[A-Za-z0-9_.@-]+$ ]] || die "invalid service name: $SERVICE_NAME"
[[ "$INSTALL_DIR" = /* ]] || die "--install-dir must be absolute"
[[ "$CONFIG_DIR" = /* ]] || die "--config-dir must be absolute"
[[ "$DATA_DIR" = /* ]] || die "--data-dir must be absolute"
[[ "$LOCAL_APP_DIR" = /* ]] || die "--local-app-dir must be absolute"
require_positive_integer_at_most "--backups-to-keep" "$BACKUPS_TO_KEEP" "$BACKUPS_TO_KEEP_MAX"

require_linux_systemd
require_root
ensure_runtime_account
build_binary_if_needed
validate_binary
detect_deploy_context
install_layout
stop_running_service_for_update
backup_existing_files
install_binary
install_config
restart_service
prune_old_backups

log "done"
