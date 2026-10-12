#!/usr/bin/env bash
# Smoke-test a local mcpmem image. This script never publishes an image.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
IMAGE="${MCPMEM_IMAGE:-mcpmem:local}"
CONFIG="${ROOT}/docker/test/mcpmem.toml"
TOKEN='container-image-smoke-token'
BUILD=false
CONTAINER=''
CONTAINER_NAME="mcpmem-image-smoke-$$"
PORT=''
TMP=''

usage() {
  printf 'Usage: MCPMEM_IMAGE=<image> %s [--build]\n' "$0" >&2
}

case "$#" in
  0) ;;
  1)
    if [[ $1 != --build ]]; then
      usage
      exit 64
    fi
    BUILD=true
    ;;
  *)
    usage
    exit 64
    ;;
esac

cleanup() {
  if [[ -n ${CONTAINER} ]]; then
    docker rm --force "${CONTAINER}" >/dev/null 2>&1 || true
  fi
  if [[ -n ${TMP} ]]; then
    rm -rf "${TMP}"
  fi
}
trap cleanup EXIT

if [[ ${BUILD} == true ]]; then
  docker build --target runtime --tag "${IMAGE}" "${ROOT}"
fi

TMP="$(mktemp -d)"
DATA_DIR="${TMP}/data"
TOKEN_FILE="${TMP}/mcpmem-auth-token"
RESTART_CONFIG="${TMP}/restart.toml"
RESPONSE_FILE="${TMP}/ui.html"
mkdir "${DATA_DIR}"
printf '%s\n' "${TOKEN}" >"${TOKEN_FILE}"
chmod 0444 "${TOKEN_FILE}"

cat >"${RESTART_CONFIG}" <<'EOF'
[server]
memory-file = "/var/lib/mcpmem/memory.mcpmem"
transport = "http"
bind = "0.0.0.0:8080"
roles = ["mcp"]
ui = true

[tools]
all = true

[security]
auth-token-file = "/run/secrets/mcpmem-auth-token"
EOF

prepare_data_dir() {
  docker run --rm --user 0:0 --entrypoint /bin/sh \
    --mount "type=bind,src=${DATA_DIR},dst=/var/lib/mcpmem" \
    "${IMAGE}" -c 'chown 10001:10001 /var/lib/mcpmem'
}

start_container() {
  local config="$1"

  CONTAINER="$(docker run --detach --name "${CONTAINER_NAME}" \
    --publish "127.0.0.1::8080" \
    --read-only \
    --tmpfs /tmp \
    --cap-drop ALL \
    --security-opt no-new-privileges:true \
    --mount "type=bind,src=${DATA_DIR},dst=/var/lib/mcpmem" \
    --mount "type=bind,src=${config},dst=/etc/mcpmem/mcpmem.toml,readonly" \
    --mount "type=bind,src=${TOKEN_FILE},dst=/run/secrets/mcpmem-auth-token,readonly" \
    "${IMAGE}")"
  PORT="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "8080/tcp") 0).HostPort}}' "${CONTAINER}")"
}

wait_for_ui() {
  local label="$1" attempt session_status status

  for ((attempt = 1; attempt <= 60; attempt++)); do
    session_status="$(curl --silent --request GET --output /dev/null \
      --write-out '%{http_code}' \
      --header "Authorization: Bearer ${TOKEN}" \
      "http://127.0.0.1:${PORT}/ui/api/session" || true)"
    : >"${RESPONSE_FILE}"
    status="$(curl --silent --request GET --output "${RESPONSE_FILE}" \
      --write-out '%{http_code}' \
      --header "Authorization: Bearer ${TOKEN}" \
      "http://127.0.0.1:${PORT}/ui" || true)"
    if [[ ${session_status} == 200 ]] && [[ ${status} == 200 ]] && grep -Fq '<div id="root">' "${RESPONSE_FILE}"; then
      printf 'PASS %s\n' "${label}"
      return
    fi
    sleep 1
  done

  printf 'The container did not serve the authenticated session API and the UI shell.\n' >&2
  docker logs "${CONTAINER}" >&2 || true
  return 1
}

stop_container() {
  docker stop "${CONTAINER}" >/dev/null
  docker rm "${CONTAINER}" >/dev/null
  CONTAINER=''
}

prepare_data_dir
start_container "${CONFIG}"
wait_for_ui 'first start'
stop_container

start_container "${RESTART_CONFIG}"
wait_for_ui 'restart with the same data directory'
stop_container
