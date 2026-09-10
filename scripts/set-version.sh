#!/usr/bin/env bash
# Set one version across the whole workspace.
#
#   scripts/set-version.sh 1.0.0-rc.2
#
# The version lives in five `[package]` blocks and in every path dependency
# requirement. Editing them by hand is how a release arrives with a tag the
# manifests do not carry: `v1.0.0-rc.2` failed its release gate because the
# workspace still said `1.0.0-rc.1`.
#
# The command is identical in Bash and fish.
set -euo pipefail

if [[ $# -ne 1 ]]; then
	printf 'usage: %s <version>\n' "$0" >&2
	exit 2
fi
new="$1"

current="$(awk '/^\[package\]/ {inpkg = 1; next}
                /^\[/ {inpkg = 0}
                inpkg && /^version = / {gsub(/[",]/, "", $3); print $3; exit}' Cargo.toml)"
if [[ -z ${current} ]]; then
	printf 'cannot read the current version from Cargo.toml\n' >&2
	exit 1
fi
if [[ ${current} == "${new}" ]]; then
	printf 'already at %s\n' "${new}"
	exit 0
fi

for manifest in Cargo.toml crates/*/Cargo.toml; do
	# The `[package]` version, first occurrence only.
	awk -v old="${current}" -v new="${new}" '
		/^\[package\]/ {inpkg = 1}
		/^\[/ && !/^\[package\]/ {inpkg = 0}
		inpkg && !done && $0 == "version = \"" old "\"" {
			print "version = \"" new "\""; done = 1; next
		}
		{print}
	' "${manifest}" >"${manifest}.tmp"
	mv "${manifest}.tmp" "${manifest}"
	# Every path dependency requirement.
	sed -i.bak "s/version = \"${current}\"/version = \"${new}\"/g" "${manifest}"
	rm -f "${manifest}.bak"
done

# Refresh Cargo.lock without touching dependency versions.
cargo update --workspace --quiet

printf '%s -> %s\n\n' "${current}" "${new}"
scripts/check-release-version.sh "v${new}"
