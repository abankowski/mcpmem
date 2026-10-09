#!/usr/bin/env bash
# Test the release CI proof helper.
#
#   scripts/test-check-release-ci.sh
#
# A release must have one successful CI push run for its exact commit.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CHECK="${HERE}/check-release-ci.sh"
TAG_SHA='0123456789abcdef0123456789abcdef01234567'
failures=0
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
mkdir -p "${tmp}/bin"

cat >"${tmp}/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$@" >"${FAKE_GH_ARGS:?}"
if [[ ${1:-} != run || ${2:-} != list ]]; then
  printf 'fake gh expected `run list`\n' >&2
  exit 2
fi
printf '%s\n' "${FAKE_GH_RESPONSE:?}"
EOF
chmod +x "${tmp}/bin/gh"

fail() {
  printf 'FAIL  %s\n' "$1" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'PASS  %s\n' "$1"
}

run_case() {
  local name="$1" response="$2" want_status="$3" args_file output status
  args_file="${tmp}/${name}.args"

  if output="$(PATH="${tmp}/bin:${PATH}" FAKE_GH_ARGS="${args_file}" FAKE_GH_RESPONSE="${response}" "${CHECK}" "${TAG_SHA}" 2>&1)"; then
    status=0
  else
    status=$?
  fi

  if [[ ${status} -ne ${want_status} ]]; then
    fail "${name}: exit ${status}, want ${want_status}; ${output}"
    return
  fi
  if [[ ${want_status} -eq 0 ]] && [[ ${output} != *"matching CI run"* ]]; then
    fail "${name}: success did not name the CI run; ${output}"
    return
  fi
  pass "${name}"
}

assert_flag_value() {
  local flag="$1" want="$2" args_file="$3" previous='' argument
  while IFS= read -r argument; do
    if [[ ${previous} == "${flag}" && ${argument} == "${want}" ]]; then
      return
    fi
    previous="${argument}"
  done <"${args_file}"
  fail "query lacks ${flag} ${want}"
}

if [[ ! -x ${CHECK} ]]; then
  fail 'check-release-ci.sh is not executable'
else
  run_case missing '[]' 1
  run_case failed "[{\"databaseId\":42,\"conclusion\":\"failure\",\"event\":\"push\",\"headBranch\":\"main\",\"headSha\":\"${TAG_SHA}\",\"status\":\"completed\"}]" 1
  run_case wrong-sha '[{"databaseId":43,"conclusion":"success","event":"push","headBranch":"main","headSha":"abcdefabcdefabcdefabcdefabcdefabcdefabcd","status":"completed"}]' 1
  run_case success "[{\"databaseId\":44,\"conclusion\":\"success\",\"event\":\"push\",\"headBranch\":\"main\",\"headSha\":\"${TAG_SHA}\",\"status\":\"completed\"}]" 0

  success_args="${tmp}/success.args"
  assert_flag_value --workflow CI "${success_args}"
  assert_flag_value --branch main "${success_args}"
  assert_flag_value --commit "${TAG_SHA}" "${success_args}"
  assert_flag_value --event push "${success_args}"
  assert_flag_value --status success "${success_args}"
fi

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
