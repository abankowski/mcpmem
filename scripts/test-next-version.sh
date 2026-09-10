#!/usr/bin/env bash
# Tests for scripts/next-version.sh.
#
#   scripts/test-next-version.sh
#
# The load-bearing case is `1.0.0-rc.9 -> 1.0.0-rc.10`. A counter bumped as text
# instead of as a number gives `rc.91`, which sorts before `rc.10` and would
# publish a candidate that looks newer than it is.
#
# The command is identical in Bash and fish.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
failures=0

expect() {
	local given="$1" want="$2" got
	got="$("${HERE}/next-version.sh" "${given}" 2>&1)" || got="ERROR: ${got}"
	if [[ ${got} == "${want}" ]]; then
		printf 'PASS  %-16s -> %s\n' "${given}" "${got}"
	else
		printf 'FAIL  %-16s -> %s, want %s\n' "${given}" "${got}" "${want}" >&2
		failures=$((failures + 1))
	fi
}

reject() {
	local given="$1"
	if "${HERE}/next-version.sh" "${given}" >/dev/null 2>&1; then
		printf 'FAIL  %-16s was accepted, want a refusal\n' "${given}" >&2
		failures=$((failures + 1))
	else
		printf 'PASS  %-16s refused\n' "${given}"
	fi
}

expect 1.0.0-rc.3 1.0.0-rc.4
expect 1.0.0-rc.9 1.0.0-rc.10      # the one that text arithmetic gets wrong
expect 1.0.0-rc.19 1.0.0-rc.20
expect 1.0.0-rc 1.0.0-rc.2
expect 2.4.0-beta.1 2.4.0-beta.2
expect 1.0.0 1.1.0
expect 1.2.3 1.3.0
expect 0.9.0 0.10.0                # minor 9 -> 10, same arithmetic trap
expect 1.9.9 1.10.0

reject 1.0                          # not a semver core
reject 1.0.0+build.7                # crates.io cannot resolve build metadata
reject 1.0.0-rc.1+build             # nor with a prerelease

# No argument reads Cargo.toml, so this case must not depend on the directory
# the test runs from. Run it where no manifest exists. The first version of the
# test asserted a refusal and passed from /tmp while failing inside the repo.
empty_run="$(cd "$(mktemp -d)" && "${HERE}/next-version.sh" 2>&1 || true)"
if [[ ${empty_run} == "cannot read a version" ]]; then
	printf 'PASS  no argument, no manifest -> refused\n'
else
	printf 'FAIL  no argument, no manifest -> %s\n' "${empty_run}" >&2
	failures=$((failures + 1))
fi

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
