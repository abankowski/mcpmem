#!/usr/bin/env bash
# Print the version that follows the one given, or the one in Cargo.toml.
#
#   scripts/next-version.sh              # from Cargo.toml
#   scripts/next-version.sh 1.0.0-rc.3   # -> 1.0.0-rc.4
#   scripts/next-version.sh 1.2.3        # -> 1.3.0
#
# The rule: a release candidate advances its counter, and a stable release
# advances the minor. After a release, `main` therefore names the version that
# is coming next, never the one already published.
#
# A patch or a major release is a human decision. Set it with
# scripts/set-version.sh.
#
# The command is identical in Bash and fish.
set -euo pipefail

current="${1:-}"
if [[ -z ${current} && -f Cargo.toml ]]; then
	current="$(awk '/^\[package\]/ {inpkg = 1; next}
	                /^\[/ {inpkg = 0}
	                inpkg && /^version = / {gsub(/[",]/, "", $3); print $3; exit}' Cargo.toml)"
fi

if [[ -z ${current} ]]; then
	printf 'cannot read a version\n' >&2
	exit 1
fi
if [[ ${current} == *+* ]]; then
	printf 'build metadata is not supported: %s\n' "${current}" >&2
	exit 1
fi

core="${current%%-*}"
pre=""
[[ ${current} == *-* ]] && pre="${current#*-}"

if [[ ! ${core} =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
	printf 'not a semver core: %s\n' "${core}" >&2
	exit 1
fi
major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"

if [[ -z ${pre} ]]; then
	# Stable: the next release is a minor one until a human says otherwise.
	printf '%s.%s.0\n' "${major}" $((minor + 1))
	exit 0
fi

# Prerelease: advance the trailing counter. `rc.9` becomes `rc.10`, not `rc.91`,
# so the counter must be split off as a whole identifier.
if [[ ${pre} =~ ^(.*)\.([0-9]+)$ ]]; then
	printf '%s-%s.%s\n' "${core}" "${BASH_REMATCH[1]}" $((BASH_REMATCH[2] + 1))
else
	# `1.0.0-rc` has no counter yet; `rc.2` follows `rc`.
	printf '%s-%s.2\n' "${core}" "${pre}"
fi
