#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  ./deploy.sh --remote <alias> --app <service_key> --file <package_file> [--deploy]
  ./deploy.sh --remote <alias> --app <service_key> --deploy [--action <action>]

Options:
  --remote <alias>        Remote alias. Reads EASY_DEPLOY_<ALIAS>_URL and EASY_DEPLOY_<ALIAS>_TOKEN.
  --url <url>             Easy Deploy base URL. Overrides remote URL.
  --token <token>         API token. Overrides remote token.
  --app <service_key>     Service/app key in Easy Deploy. Alias: --service.
  --file <path>           Package file to upload.
  --entry-file <path>     Entry file inside archive, for example bin/server.
  --version <version>     Explicit release version. Usually parsed from package name.
  --version-code <code>   Explicit versionCode. Usually parsed from package name.
  --published-at <time>   Publish time, for example 2026-06-12T10:00:00Z.
  --source <source>       Package source label. Default: script.
  --deploy                Deploy after upload, or deploy existing current release when --file is absent.
  --action <action>       Deploy action when --file is absent. Default: binary_restart.
  --config <file>         Config file to source. Default: .deploy.env.
  --dry-run               Print resolved request plan without sending requests.
  -h, --help              Show help.

Examples:
  ./deploy.sh --remote local --app orders-api-prod --file dist/orders-api-prod_version_1_2_3.tar.gz
  ./deploy.sh --remote prod --app orders-api-prod --file dist/orders-api-prod_version_1_2_3.tar.gz --deploy
  ./deploy.sh --remote prod --app orders-api-prod --deploy --action binary_restart

Config:
  Copy .deploy.env.example to .deploy.env and fill API URLs/tokens.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_next() {
  local name="$1"
  local value="${2:-}"
  if [[ -z "$value" || "$value" == --* ]]; then
    die "$name requires a value"
  fi
  printf '%s' "$value"
}

normalize_remote_key() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]' | sed 's/[^A-Z0-9_]/_/g'
}

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  printf '%s' "$value"
}

run_request() {
  local response status body
  response="$(curl -sS -w $'\n%{http_code}' "$@")" || die "curl request failed"
  status="${response##*$'\n'}"
  body="${response%$'\n'$status}"
  printf '%s\n' "$body"
  if [[ ! "$status" =~ ^2[0-9][0-9]$ ]]; then
    die "request failed with HTTP $status"
  fi
}

CONFIG_FILE="${EASY_DEPLOY_CONFIG:-.deploy.env}"
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  case "${args[$i]}" in
    --config)
      CONFIG_FILE="$(need_next --config "${args[$((i + 1))]:-}")"
      ;;
    --config=*)
      CONFIG_FILE="${args[$i]#*=}"
      ;;
  esac
done

if [[ -f "$CONFIG_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$CONFIG_FILE"
  set +a
fi

REMOTE="${EASY_DEPLOY_REMOTE:-local}"
BASE_URL="${EASY_DEPLOY_URL:-}"
TOKEN="${EASY_DEPLOY_TOKEN:-}"
SERVICE_KEY="${EASY_DEPLOY_SERVICE_KEY:-${EASY_DEPLOY_APP:-}}"
PACKAGE_FILE=""
ENTRY_FILE="${EASY_DEPLOY_ENTRY_FILE:-}"
RELEASE_VERSION="${EASY_DEPLOY_RELEASE_VERSION:-}"
VERSION_CODE="${EASY_DEPLOY_VERSION_CODE:-}"
PUBLISHED_AT="${EASY_DEPLOY_PUBLISHED_AT:-}"
SOURCE_LABEL="${EASY_DEPLOY_SOURCE:-script}"
DEPLOY=false
ACTION="${EASY_DEPLOY_DEPLOY_ACTION:-binary_restart}"
DRY_RUN=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --remote)
      REMOTE="$(need_next --remote "${2:-}")"
      shift 2
      ;;
    --remote=*)
      REMOTE="${1#*=}"
      shift
      ;;
    --url)
      BASE_URL="$(need_next --url "${2:-}")"
      shift 2
      ;;
    --url=*)
      BASE_URL="${1#*=}"
      shift
      ;;
    --token)
      TOKEN="$(need_next --token "${2:-}")"
      shift 2
      ;;
    --token=*)
      TOKEN="${1#*=}"
      shift
      ;;
    --app|--service)
      SERVICE_KEY="$(need_next "$1" "${2:-}")"
      shift 2
      ;;
    --app=*|--service=*)
      SERVICE_KEY="${1#*=}"
      shift
      ;;
    --file)
      PACKAGE_FILE="$(need_next --file "${2:-}")"
      shift 2
      ;;
    --file=*)
      PACKAGE_FILE="${1#*=}"
      shift
      ;;
    --entry-file)
      ENTRY_FILE="$(need_next --entry-file "${2:-}")"
      shift 2
      ;;
    --entry-file=*)
      ENTRY_FILE="${1#*=}"
      shift
      ;;
    --version|--release-version)
      RELEASE_VERSION="$(need_next "$1" "${2:-}")"
      shift 2
      ;;
    --version=*|--release-version=*)
      RELEASE_VERSION="${1#*=}"
      shift
      ;;
    --version-code|--versionCode)
      VERSION_CODE="$(need_next "$1" "${2:-}")"
      shift 2
      ;;
    --version-code=*|--versionCode=*)
      VERSION_CODE="${1#*=}"
      shift
      ;;
    --published-at|--publishedAt)
      PUBLISHED_AT="$(need_next "$1" "${2:-}")"
      shift 2
      ;;
    --published-at=*|--publishedAt=*)
      PUBLISHED_AT="${1#*=}"
      shift
      ;;
    --source)
      SOURCE_LABEL="$(need_next --source "${2:-}")"
      shift 2
      ;;
    --source=*)
      SOURCE_LABEL="${1#*=}"
      shift
      ;;
    --deploy)
      DEPLOY=true
      shift
      ;;
    --no-deploy)
      DEPLOY=false
      shift
      ;;
    --action)
      ACTION="$(need_next --action "${2:-}")"
      shift 2
      ;;
    --action=*)
      ACTION="${1#*=}"
      shift
      ;;
    --config)
      shift 2
      ;;
    --config=*)
      shift
      ;;
    --dry-run)
      DRY_RUN=true
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

REMOTE_KEY="$(normalize_remote_key "$REMOTE")"
REMOTE_URL_VAR="EASY_DEPLOY_${REMOTE_KEY}_URL"
REMOTE_TOKEN_VAR="EASY_DEPLOY_${REMOTE_KEY}_TOKEN"

if [[ -z "$BASE_URL" ]]; then
  BASE_URL="${!REMOTE_URL_VAR:-}"
fi
if [[ -z "$TOKEN" ]]; then
  TOKEN="${!REMOTE_TOKEN_VAR:-}"
fi
if [[ -z "$BASE_URL" && "$REMOTE" == "local" ]]; then
  BASE_URL="http://127.0.0.1:9066"
fi

BASE_URL="${BASE_URL%/}"

[[ -n "$BASE_URL" ]] || die "missing URL: set --url or EASY_DEPLOY_${REMOTE_KEY}_URL"
[[ -n "$TOKEN" ]] || die "missing token: set --token or EASY_DEPLOY_${REMOTE_KEY}_TOKEN"
[[ -n "$SERVICE_KEY" ]] || die "missing app key: set --app <service_key>"

case "$ACTION" in
  up|down|restart|compose_up|compose_down|compose_restart|binary_restart|binary_stop) ;;
  *) die "unsupported --action: $ACTION" ;;
esac

if [[ -z "$PACKAGE_FILE" && "$DEPLOY" != true ]]; then
  die "nothing to do: provide --file to upload and/or --deploy to create a deploy task"
fi

if [[ -n "$PACKAGE_FILE" ]]; then
  [[ -f "$PACKAGE_FILE" ]] || die "package file not found: $PACKAGE_FILE"
  package_name="$(basename "$PACKAGE_FILE")"
  case "$package_name" in
    "${SERVICE_KEY}_version_"*) ;;
    *)
      die "package name must start with ${SERVICE_KEY}_version_, for example ${SERVICE_KEY}_version_1_2_3.tar.gz"
      ;;
  esac

  upload_url="${BASE_URL}/api/v1/services/${SERVICE_KEY}/packages"
  if [[ "$DRY_RUN" == true ]]; then
    printf 'POST %s\n' "$upload_url"
    printf 'Authorization: Bearer ***\n'
    printf 'package_file=%s\n' "$PACKAGE_FILE"
    printf 'entry_file=%s\n' "$ENTRY_FILE"
    printf 'release_version=%s\n' "$RELEASE_VERSION"
    printf 'versionCode=%s\n' "$VERSION_CODE"
    printf 'publishedAt=%s\n' "$PUBLISHED_AT"
    printf 'source=%s\n' "$SOURCE_LABEL"
    printf 'auto_deploy=%s\n' "$DEPLOY"
  else
    curl_args=(
      -X POST "$upload_url"
      -H "Authorization: Bearer $TOKEN"
      -F "package_file=@${PACKAGE_FILE}"
      -F "source=${SOURCE_LABEL}"
      -F "auto_deploy=${DEPLOY}"
    )
    [[ -n "$ENTRY_FILE" ]] && curl_args+=(-F "entry_file=${ENTRY_FILE}")
    [[ -n "$RELEASE_VERSION" ]] && curl_args+=(-F "release_version=${RELEASE_VERSION}")
    [[ -n "$VERSION_CODE" ]] && curl_args+=(-F "versionCode=${VERSION_CODE}")
    [[ -n "$PUBLISHED_AT" ]] && curl_args+=(-F "publishedAt=${PUBLISHED_AT}")

    printf 'Uploading %s to %s (%s)\n' "$PACKAGE_FILE" "$REMOTE" "$SERVICE_KEY"
    run_request "${curl_args[@]}"
  fi
elif [[ "$DEPLOY" == true ]]; then
  deploy_url="${BASE_URL}/api/v1/services/${SERVICE_KEY}/deploy"
  payload="{\"action\":\"$(json_escape "$ACTION")\"}"
  if [[ "$DRY_RUN" == true ]]; then
    printf 'POST %s\n' "$deploy_url"
    printf 'Authorization: Bearer ***\n'
    printf '%s\n' "$payload"
  else
    printf 'Creating deploy task on %s (%s, action=%s)\n' "$REMOTE" "$SERVICE_KEY" "$ACTION"
    run_request \
      -X POST "$deploy_url" \
      -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" \
      -d "$payload"
  fi
fi
