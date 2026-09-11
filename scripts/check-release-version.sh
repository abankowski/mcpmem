#!/usr/bin/env bash
# Verify that a release tag, the workspace versions and crates.io agree.
#
#   scripts/check-release-version.sh              # workspace consistency only
#   scripts/check-release-version.sh v1.0.0       # also match the tag
#   scripts/check-release-version.sh --registry   # also require the version to be unpublished
#
# Ordinary CI runs the consistency checks. Only a release run adds
# `--registry`, because main keeps a published version number between two
# releases.
#
# The command is identical in Bash and fish.
set -uo pipefail

CRATES=(mcpmem-core mcpmem-runtime mcpmem-indexer mcpmem-webhook mcpmem-oauth mcpmem)
UA='mcpmem-release-check (https://github.com/abankowski/mcpmem)'
failures=0
check_registry=0
tag=''

for argument in "$@"; do
	case "${argument}" in
	--registry) check_registry=1 ;;
	-*)
		printf 'unknown option: %s\n' "${argument}" >&2
		exit 2
		;;
	*) tag="${argument}" ;;
	esac
done

fail() {
	printf 'FAIL  %s\n' "$1" >&2
	failures=$((failures + 1))
}
ok() { printf 'ok    %s\n' "$1"; }

manifest_of() {
	case "$1" in
	mcpmem) printf 'Cargo.toml\n' ;;
	*) printf 'crates/%s/Cargo.toml\n' "$1" ;;
	esac
}

version_of() {
	awk '/^\[package\]/ {inpkg = 1; next}
	     /^\[/ {inpkg = 0}
	     inpkg && /^version = / {gsub(/[",]/, "", $3); print $3; exit}' "$(manifest_of "$1")"
}

# Strict semver 2.0.0, minus build metadata. crates.io stores a version with
# build metadata but no dependency can request it, so the release becomes
# unreachable. Reject it here instead.
SEMVER='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?$'

root_version="$(version_of mcpmem)"
if [[ -z ${root_version} ]]; then
	fail 'Cargo.toml has no [package] version'
	exit 1
fi

if [[ ${root_version} == *+* ]]; then
	fail "version ${root_version} carries build metadata; crates.io cannot resolve it"
elif [[ ! ${root_version} =~ ${SEMVER} ]]; then
	fail "version ${root_version} is not valid semver 2.0.0"
else
	ok "version ${root_version} is valid semver"
fi

for crate in "${CRATES[@]}"; do
	found="$(version_of "${crate}")"
	if [[ ${found} != "${root_version}" ]]; then
		fail "${crate} is ${found:-missing}, expected ${root_version}"
	fi
done
[[ ${failures} -eq 0 ]] && ok "all ${#CRATES[@]} workspace crates are ${root_version}"

# Every path dependency must also carry that version requirement, or the
# published crate resolves against whatever crates.io already holds.
while read -r line; do
	fail "path dependency without a version requirement: ${line}"
done < <(grep -rn 'path = "\(crates/\)\?\.\?\.*/*mcpmem-' Cargo.toml crates/*/Cargo.toml |
	grep -v 'version = ')

for crate in "${CRATES[@]}"; do
	requested="$(grep -o "${crate} = { path = [^}]*version = \"[^\"]*\"" Cargo.toml crates/*/Cargo.toml |
		grep -o 'version = "[^"]*"' | grep -o '"[^"]*"' | tr -d '"' | sort -u)"
	if [[ -n ${requested} && ${requested} != "${root_version}" ]]; then
		fail "path dependencies request ${crate} $(echo "${requested}" | tr '\n' ' '), expected ${root_version}"
	fi
done

if [[ -n ${tag} ]]; then
	if [[ ${tag} != v* ]]; then
		fail "tag ${tag} must start with 'v'"
	elif [[ ${tag#v} != "${root_version}" ]]; then
		fail "tag ${tag} does not match version ${root_version}"
	else
		ok "tag ${tag} matches version ${root_version}"
	fi
fi

# A published version is immutable, so a release that repeats one can only be a
# mistake.
if [[ ${check_registry} -eq 1 ]]; then
	for crate in "${CRATES[@]}"; do
		body="$(curl -sS -H "User-Agent: ${UA}" "https://crates.io/api/v1/crates/${crate}/${root_version}" 2>/dev/null)"
		case "${body}" in
		*'"num":"'"${root_version}"'"'*) fail "${crate} ${root_version} is already on crates.io" ;;
		*) ok "${crate} ${root_version} is free on crates.io" ;;
		esac
	done
fi

if [[ ${failures} -gt 0 ]]; then
	printf '\n%d check(s) failed\n' "${failures}" >&2
	exit 1
fi
printf '\nrelease version %s is consistent\n' "${root_version}"
