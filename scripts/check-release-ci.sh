#!/usr/bin/env bash
# Verify that CI passed for the exact release commit on main.
#
#   scripts/check-release-ci.sh <tag-sha>
#
# The release workflow runs this after it proves the tag commit is on main.
set -euo pipefail

if [[ $# -ne 1 ]] || [[ ! $1 =~ ^[0-9a-f]{40}$ ]]; then
  printf 'usage: %s <40-character lowercase commit SHA>\n' "$0" >&2
  exit 2
fi

tag_sha="$1"
runs="$(gh run list \
  --workflow CI \
  --branch main \
  --commit "${tag_sha}" \
  --event push \
  --status success \
  --limit 1 \
  --json databaseId,conclusion,event,headBranch,headSha,status)"

if ! printf '%s\n' "${runs}" | jq -e --arg sha "${tag_sha}" '
  length == 1 and
  .[0].conclusion == "success" and
  .[0].event == "push" and
  .[0].headBranch == "main" and
  .[0].headSha == $sha and
  .[0].status == "completed"
' >/dev/null; then
  printf 'no completed successful CI push run on main for %s\n' "${tag_sha}" >&2
  exit 1
fi

run_id="$(printf '%s\n' "${runs}" | jq -r '.[0].databaseId')"
printf 'matching CI run %s passed for %s\n' "${run_id}" "${tag_sha}"
