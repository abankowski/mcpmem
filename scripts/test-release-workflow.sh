#!/usr/bin/env bash
# Test the release workflow graph.
#
#   scripts/test-release-workflow.sh
#
# Archives must exist before publishing. Release assets must upload after the
# crates publish step succeeds.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORKFLOW="${HERE}/../.github/workflows/release.yml"
failures=0

fail() {
  printf 'FAIL  %s\n' "$1" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'PASS  %s\n' "$1"
}

job_body() {
  local job="$1"
  awk -v job="${job}" '
    $0 == "  " job ":" { in_job = 1; next }
    in_job && /^  [A-Za-z0-9_-]+:/ { exit }
    in_job { print }
  ' "${WORKFLOW}"
}

assert_need() {
  local job="$1" dependency="$2" body
  body="$(job_body "${job}")"
  if [[ -z ${body} ]]; then
    fail "${job} job does not exist"
  elif printf '%s\n' "${body}" | grep -Fqx "    needs: [${dependency}]"; then
    pass "${job} waits for ${dependency}"
  else
    fail "${job} does not wait for ${dependency}"
  fi
}

assert_need publish archive
assert_need upload-binaries publish

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
