#!/usr/bin/env bash
# Test Cargo resolution waits and release flags for each crate.
#
#   scripts/test-publish-crates.sh
#
# Library packages skip verification after CI proves the release commit. The
# root package keeps verification and permits the built UI bundle. The fake
# API exposes mcpmem-runtime after its publish. Cargo accepts mcpmem on its
# third preflight dry run.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
PUBLISH="${HERE}/publish-crates.sh"
failures=0
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
mkdir -p "${tmp}/bin" "${tmp}/state"

cat >"${tmp}/bin/curl" <<'EOF'
#!/usr/bin/env bash
url="${!#}"
crate_version="${url#https://crates.io/api/v1/crates/}"
crate="${crate_version%%/*}"
version="${crate_version#*/}"

if [[ -f "${FAKE_STATE_DIR:?}/mcpmem-runtime-submitted" ]]; then
  touch "${FAKE_STATE_DIR}/runtime-api-visible"
fi

if [[ ${crate} == "mcpmem-runtime" && -f "${FAKE_STATE_DIR:?}/runtime-api-visible" ]]; then
  printf '{"num":"%s"}\n' "${version}"
elif [[ -f "${FAKE_STATE_DIR:?}/${crate}-submitted" ]]; then
  printf '{"num":"%s"}\n' "${version}"
else
  printf '{}\n'
fi
EOF
cat >"${tmp}/bin/cargo" <<'EOF'
#!/usr/bin/env bash
args="$*"
crate=
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    -p)
      crate="$2"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    *)
      shift
      ;;
  esac
done

printf '%s\n' "${args}" >>"${FAKE_CARGO_LOG:?}"
if [[ ${crate} == "mcpmem" ]]; then
  if [[ ! -f "${FAKE_STATE_DIR:?}/runtime-api-visible" ]]; then
    printf 'mcpmem Cargo resolver has no runtime package\n' >&2
    exit 1
  fi
  if [[ ${dry_run} -eq 1 ]]; then
    attempts=0
    if [[ -f "${FAKE_STATE_DIR}/root-dry-run-attempts" ]]; then
      attempts="$(<"${FAKE_STATE_DIR}/root-dry-run-attempts")"
    fi
    attempts=$((attempts + 1))
    printf '%s\n' "${attempts}" >"${FAKE_STATE_DIR}/root-dry-run-attempts"
    if [[ ${attempts} -le 2 ]]; then
      printf 'mcpmem Cargo resolver has not updated yet\n' >&2
      exit 1
    fi
    touch "${FAKE_STATE_DIR}/root-cargo-resolved"
  elif [[ ! -f "${FAKE_STATE_DIR}/root-cargo-resolved" ]]; then
    printf 'mcpmem published before Cargo resolved its dependencies\n' >&2
    exit 1
  fi
fi

if [[ ${dry_run} -eq 0 ]]; then
  touch "${FAKE_STATE_DIR:?}/${crate}-submitted"
fi
EOF
cat >"${tmp}/bin/sleep" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${FAKE_SLEEP_LOG:?}"
EOF
chmod +x "${tmp}/bin/curl" "${tmp}/bin/cargo" "${tmp}/bin/sleep"

fail() {
  printf 'FAIL  %s\n' "$1" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'PASS  %s\n' "$1"
}

cargo_call_count() {
  local crate="$1" log="$2"
  grep -Ec "^publish -p ${crate}( |$)" "${log}" || true
}

cargo_dry_run_count() {
  local crate="$1" log="$2"
  grep -E "^publish -p ${crate}( |$)" "${log}" | grep -cE ' --dry-run$' || true
}

actual_publish_line() {
  local crate="$1" log="$2"
  grep -E "^publish -p ${crate}( |$)" "${log}" | grep -vE ' --dry-run$' || true
}

dry_run_lines() {
  local crate="$1" log="$2"
  grep -E "^publish -p ${crate}( |$)" "${log}" | grep -E ' --dry-run$' || true
}

assert_preflight_runs() {
  local crate="$1" expected_dry_runs="$2" log="$3"
  local actual dry_runs actual_count dry_run_count dry_run
  actual="$(actual_publish_line "${crate}" "${log}")"
  dry_runs="$(dry_run_lines "${crate}" "${log}")"
  actual_count="$(cargo_call_count "${crate}" "${log}")"
  dry_run_count="$(cargo_dry_run_count "${crate}" "${log}")"

  if [[ ${actual_count} -ne $((expected_dry_runs + 1)) ]]; then
    fail "${crate} ran ${actual_count} times, want $((expected_dry_runs + 1))"
  elif [[ ${dry_run_count} -ne ${expected_dry_runs} ]]; then
    fail "${crate} ran ${dry_run_count} preflight dry runs, want ${expected_dry_runs}"
  elif [[ -z ${actual} ]]; then
    fail "${crate} did not publish after its preflight"
  else
    while IFS= read -r dry_run; do
      if [[ ${dry_run% --dry-run} != "${actual}" ]]; then
        fail "${crate} preflight flags differ from publish flags: ${dry_run}"
        return
      fi
    done <<<"${dry_runs}"
    pass "${crate} uses its publish flags for preflight"
  fi
}

assert_library_flags() {
  local crate="$1" log="$2" line
  line="$(actual_publish_line "${crate}" "${log}")"
  if [[ " ${line} " != *' --locked '* ]] || [[ " ${line} " != *' --no-verify '* ]]; then
    fail "${crate} lacks --locked or --no-verify: ${line}"
  else
    pass "${crate} uses --no-verify"
  fi
}

assert_root_flags() {
  local log="$1" line
  line="$(actual_publish_line mcpmem "${log}")"
  if [[ " ${line} " == *' --no-verify '* ]]; then
    fail "mcpmem must retain verification: ${line}"
  elif [[ " ${line} " != *' --locked '* ]] || [[ " ${line} " != *' --allow-dirty '* ]]; then
    fail "mcpmem lacks --locked or --allow-dirty: ${line}"
  else
    pass 'mcpmem retains verification'
  fi
}

assert_root_resolution_retries() {
  local attempts sleep_calls
  if [[ ! -f ${tmp}/state/runtime-api-visible ]]; then
    fail 'the API did not report mcpmem-runtime as visible'
  elif [[ ! -f ${tmp}/state/root-cargo-resolved ]]; then
    fail 'Cargo did not resolve mcpmem before it published'
  else
    attempts="$(<"${tmp}/state/root-dry-run-attempts")"
    sleep_calls="$(wc -l <"${tmp}/sleep.log")"
    if [[ ${attempts} -ne 3 ]]; then
      fail "mcpmem ran ${attempts} preflight dry runs, want 3"
    elif [[ ${sleep_calls} -ne 2 ]]; then
      fail "Cargo resolution wait slept ${sleep_calls} times, want 2"
    else
      pass 'mcpmem waits for Cargo to resolve mcpmem-runtime'
    fi
  fi
}

assert_retry_output() {
  if [[ ${output} != *'wait     mcpmem Cargo cannot resolve dependencies'* ]]; then
    fail 'Cargo resolution wait did not report the retry'
  elif [[ ${output} == *'not visible on crates.io'* ]]; then
    fail 'the retry still reports crates.io API visibility'
  else
    pass 'Cargo resolution wait reports the resolver state'
  fi
}

run_publish() {
  local cargo_log="$1" sleep_log="$2"
  shift 2
  (
    cd "${ROOT}"
    PATH="${tmp}/bin:${PATH}" \
      FAKE_CARGO_LOG="${cargo_log}" \
      FAKE_SLEEP_LOG="${sleep_log}" \
      FAKE_STATE_DIR="${tmp}/state" \
      "${PUBLISH}" "$@"
  ) 2>&1
}

assert_dry_run_behavior() {
  local calls
  calls="$(wc -l <"${tmp}/dry-cargo.log")"
  if [[ ${calls} -ne 7 ]]; then
    fail "dry-run invoked cargo ${calls} times, want 7"
  elif grep -qvE ' --dry-run$' "${tmp}/dry-cargo.log"; then
    fail 'dry-run invoked cargo without --dry-run'
  elif [[ -f ${tmp}/dry-sleep.log ]]; then
    fail 'dry-run retried Cargo resolution'
  elif compgen -G "${tmp}/state/*-submitted" >/dev/null; then
    fail 'dry-run submitted a crate'
  else
    pass 'dry-run keeps its single-pass behavior'
  fi
}

if [[ ! -x ${PUBLISH} ]]; then
  fail 'publish-crates.sh is not executable'
elif ! output="$(run_publish "${tmp}/cargo.log" "${tmp}/sleep.log")"; then
  fail "publish-crates.sh failed: ${output}"
else
  assert_preflight_runs mcpmem-core 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem-extractor 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem-runtime 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem-indexer 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem-webhook 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem-oauth 1 "${tmp}/cargo.log"
  assert_preflight_runs mcpmem 3 "${tmp}/cargo.log"
  assert_library_flags mcpmem-core "${tmp}/cargo.log"
  assert_library_flags mcpmem-extractor "${tmp}/cargo.log"
  assert_library_flags mcpmem-runtime "${tmp}/cargo.log"
  assert_library_flags mcpmem-indexer "${tmp}/cargo.log"
  assert_library_flags mcpmem-webhook "${tmp}/cargo.log"
  assert_library_flags mcpmem-oauth "${tmp}/cargo.log"
  assert_root_flags "${tmp}/cargo.log"
  assert_root_resolution_retries
  assert_retry_output
fi

rm -rf "${tmp}/state"
mkdir -p "${tmp}/state"
if ! dry_output="$(run_publish "${tmp}/dry-cargo.log" "${tmp}/dry-sleep.log" --dry-run)"; then
  fail "publish-crates.sh --dry-run failed: ${dry_output}"
else
  assert_dry_run_behavior
fi

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
