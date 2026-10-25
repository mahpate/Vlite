#!/bin/sh
# Bootstrap Docker and run published VLite images on Ubuntu or an existing Docker host.
set -eu

VERSION=v0.4.1
DEPLOY_DIR=${HOME}/vlite-deploy
API_PORT=18084
DASHBOARD_PORT=8081
DIMENSIONS=3
YES=0
PREPARE_ONLY=0
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

fail() { printf '%s\n' "ERROR: $*" >&2; exit 1; }
confirm() {
    [ "$YES" -eq 1 ] && return 0
    printf '%s [y/N] ' "$1"
    read -r answer || fail 'Installation needs confirmation. Run in a terminal, or use --yes after reviewing the script.'
    case "$answer" in y|Y|yes|YES) return 0 ;; *) fail 'Installation declined; no packages were installed.' ;; esac
}
as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"
    else command -v sudo >/dev/null 2>&1 || fail 'sudo is required to install packages. Ask your administrator.'
        sudo "$@"
    fi
}
ubuntu_required() {
    [ -r /etc/os-release ] || fail 'Automatic installation requires Ubuntu 22.04 or 24.04.'
    . /etc/os-release
    [ "$ID" = ubuntu ] || fail 'Automatic installation supports Ubuntu only. Install Docker Engine and Compose v2 for your OS, then rerun.'
    case "$VERSION_ID" in 22.04|24.04) ;; *) fail "Ubuntu $VERSION_ID is not supported by this installer. Use Ubuntu 22.04/24.04 or install Docker and Compose manually." ;; esac
}
compose_ready() {
    command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1
}
prepare_docker() {
    if ! compose_ready; then
        ubuntu_required
        if command -v docker >/dev/null 2>&1 &&
            dpkg-query -W -f='${Status}\n' docker-ce 2>/dev/null | grep -q 'install ok installed'; then
            confirm 'Install the missing Compose plugin for your existing Docker CE installation?'
            as_root apt-get update
            as_root apt-get install -y docker-compose-plugin
        # Do not replace a different container runtime or Docker Desktop integration.
        elif command -v docker >/dev/null 2>&1 ||
            dpkg-query -W -f='${Status}\n' docker.io docker-ce containerd containerd.io podman-docker 2>/dev/null | grep -q 'install ok installed'; then
            fail 'An existing Docker/container runtime needs attention. Install/repair its Compose v2 plugin or enable Docker Desktop WSL integration; this script will not replace it.'
        else
        confirm 'Install Docker Engine and Compose from the official Docker Ubuntu repository? This changes system packages and starts Docker.'
        as_root apt-get update
        as_root apt-get install -y ca-certificates curl
        as_root install -m 0755 -d /etc/apt/keyrings
        key=$(mktemp)
        curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o "$key"
        as_root install -m 0644 "$key" /etc/apt/keyrings/docker.asc
        rm -f "$key"
        arch=$(dpkg --print-architecture)
        source_file=$(mktemp)
        printf 'deb [arch=%s signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu %s stable\n' "$arch" "$VERSION_CODENAME" > "$source_file"
        as_root install -m 0644 "$source_file" /etc/apt/sources.list.d/docker.list
        rm -f "$source_file"
        as_root apt-get update
        as_root apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
        fi
    fi
    compose_ready || fail 'Docker Compose v2 is unavailable after installation.'
    docker_version=$(docker compose version --short)
    docker_version=${docker_version#v}
    major=${docker_version%%.*}
    rest=${docker_version#*.}
    minor=${rest%%.*}
    case "$major:$minor" in *[!0-9:]*|:*|*:) fail "Cannot parse Docker Compose version: $docker_version" ;; esac
    [ "$major" -gt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -ge 20 ]; } ||
        fail 'Docker Compose 2.20 or newer is required for --wait. Update Docker/Compose and rerun.'
    if ! docker info >/dev/null 2>&1; then
        if [ "$(id -u)" -ne 0 ]; then
            fail 'Docker is stopped or your user cannot access it. Start Docker Desktop/Engine, or rerun with sudo for a local root-owned Docker Engine. No group permissions were changed.'
        fi
        if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
            systemctl start docker
        elif command -v service >/dev/null 2>&1; then
            service docker start
        else
            fail 'Start Docker Desktop or Docker Engine, then rerun.'
        fi
        docker info >/dev/null 2>&1 || fail 'Docker did not become ready. Check virtualization/WSL 2 and the Docker service logs.'
    fi
    [ "$(docker info --format '{{.OSType}}')" = linux ] || fail 'Switch Docker Desktop to Linux containers, then rerun.'
}
require_value() { [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value."; }
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) require_value "$@"; VERSION=$2; shift 2 ;;
        --deploy-dir) require_value "$@"; DEPLOY_DIR=$2; shift 2 ;;
        --api-port) require_value "$@"; API_PORT=$2; shift 2 ;;
        --dashboard-port) require_value "$@"; DASHBOARD_PORT=$2; shift 2 ;;
        --dimensions) require_value "$@"; DIMENSIONS=$2; shift 2 ;;
        --yes) YES=1; shift ;;
        --prepare-only) PREPARE_ONLY=1; shift ;;
        --help) printf '%s\n' 'Usage: sh docker-dashboard.sh [--yes] [--version TAG] [--deploy-dir PATH] [--api-port PORT] [--dashboard-port PORT] [--dimensions N] [--prepare-only]'; exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done
case "$VERSION" in ''|*[!a-zA-Z0-9._-]*) fail 'Version must be an image tag.' ;; esac
for number in "$API_PORT" "$DASHBOARD_PORT" "$DIMENSIONS"; do
    case "$number" in ''|*[!0-9]*) fail 'Ports and dimensions must be positive integers.' ;; esac
    [ "${#number}" -le 5 ] && [ "$number" -gt 0 ] && [ "$number" -le 65535 ] || fail 'Ports/dimensions must be between 1 and 65535.'
done
[ "$API_PORT" -ne "$DASHBOARD_PORT" ] || fail 'API and dashboard ports must differ.'
prepare_docker
[ "$PREPARE_ONLY" -eq 0 ] || exit 0
if ! command -v curl >/dev/null 2>&1; then
    ubuntu_required
    confirm 'Install curl and CA certificates to download VLite deployment files?'
    as_root apt-get update
    as_root apt-get install -y ca-certificates curl
fi
mkdir -p "$DEPLOY_DIR/.secrets"
chmod 700 "$DEPLOY_DIR" "$DEPLOY_DIR/.secrets"
if [ -f "$SCRIPT_DIR/../compose.yaml" ]; then
    cp "$SCRIPT_DIR/../compose.yaml" "$DEPLOY_DIR/compose.yaml"
else
    curl -fsSL "https://raw.githubusercontent.com/mahpate/Vlite/$VERSION/compose.yaml" -o "$DEPLOY_DIR/compose.yaml"
fi
cd "$DEPLOY_DIR"
if [ ! -s .secrets/api-token ]; then
    (umask 077; head -c 32 /dev/urandom | base64 | tr -d '\n' > .secrets/api-token)
fi
# Container UID 10001 needs file access; the enclosing directories stay private.
chmod 644 .secrets/api-token
cat > .env <<EOF
VLITE_PORT=$API_PORT
VLITE_DASHBOARD_PORT=$DASHBOARD_PORT
VLITE_DIMENSIONS=$DIMENSIONS
VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api:$VERSION
VLITE_DASHBOARD_IMAGE=ghcr.io/mahpate/vlite-dashboard:$VERSION
EOF
docker compose -f compose.yaml --profile dashboard pull --quiet api dashboard
docker compose -f compose.yaml --profile dashboard up -d --no-build --wait api dashboard
curl --fail --silent --show-error --retry 5 --retry-delay 2 "http://127.0.0.1:$API_PORT/health" >/dev/null
curl --fail --silent --show-error --retry 5 --retry-delay 2 "http://127.0.0.1:$DASHBOARD_PORT/" >/dev/null
printf '\nDashboard: http://127.0.0.1:%s\nAPI: http://127.0.0.1:%s\n' "$DASHBOARD_PORT" "$API_PORT"
printf 'To view your token locally: cat "%s/.secrets/api-token"\nPaste the value into the dashboard API token field, without "Bearer". Do not share it.\n' "$DEPLOY_DIR"
printf 'Data is stored in Docker volumes. Stop without deleting data: cd "%s" && docker compose -f compose.yaml --profile dashboard down\n' "$DEPLOY_DIR"
