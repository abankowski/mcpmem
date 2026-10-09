#!/usr/bin/env bash
# Test the cargo flags for each release crate.
#
#   scripts/test-publish-crates.sh
#
# Library packages skip verification after CI proves the release commit. The
# root package keeps verification and permits the built UI bundle.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
PUBLISH="${HERE}/publish-crates.sh"
failures=0
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
mkdir -p "${tmp}/bin"

cat >"${tmp}/bin/curl" <<'EOF'
#!/usr/bin/env bash
printf '{}\n'
EOF
cat >"${tmp}/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${FAKE_CARGO_LOG:?}"
EOF
chmod +x "${tmp}/bin/curl" "${tmp}/bin/cargo"

fail() {
  printf 'FAIL  %s\n' "$1" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'PASS  %s\n' "$1"
}

crate_line() {
  local crate="$1"
  grep -E "^publish -p ${crate}( |$)" "${tmp}/cargo.log" || true
}

assert_library_flags() {
  local crate="$1" line
  line="$(crate_line "${crate}")"
  if [[ -z ${line} ]]; then
    fail "${crate} did not publish"
  elif [[ " ${line} " != *' --locked '* ]] || [[ " ${line} " != *' --no-verify '* ]]; then
    fail "${crate} lacks --locked or --no-verify: ${line}"
  else
    pass "${crate} uses --no-verify"
  fi
}

assert_root_flags() {
  local line
  line="$(crate_line mcpmem)"
  if [[ -z ${line} ]]; then
    fail 'mcpmem did not publish'
  elif [[ " ${line} " == *' --no-verify '* ]]; then
    fail "mcpmem must retain verification: ${line}"
  elif [[ " ${line} " != *' --locked '* ]] || [[ " ${line} " != *' --allow-dirty '* ]]; then
    fail "mcpmem lacks --locked or --allow-dirty: ${line}"
  else
    pass 'mcpmem retains verification'
  fi
}

if [[ ! -x ${PUBLISH} ]]; then
  fail 'publish-crates.sh is not executable'
elif ! output="$(cd "${ROOT}" && PATH="${tmp}/bin:${PATH}" FAKE_CARGO_LOG="${tmp}/cargo.log" "${PUBLISH}" 2>&1)"; then
  fail "publish-crates.sh failed: ${output}"
else
  expected_calls=7
  actual_calls="$(wc -l <"${tmp}/cargo.log")"
  if [[ ${actual_calls} -ne ${expected_calls} ]]; then
    fail "cargo ran ${actual_calls} times, want ${expected_calls}"
  else
    pass 'cargo runs once for each crate'
  fi

  assert_library_flags mcpmem-core
  assert_library_flags mcpmem-extractor
  assert_library_flags mcpmem-runtime
  assert_library_flags mcpmem-indexer
  assert_library_flags mcpmem-webhook
  assert_library_flags mcpmem-oauth
  assert_root_flags
fi

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
