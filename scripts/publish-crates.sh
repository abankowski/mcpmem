#!/usr/bin/env bash
# Publish the workspace to crates.io in dependency order.
#
#   scripts/publish-crates.sh              # publish
#   scripts/publish-crates.sh --dry-run    # package and verify only
#
# The script skips a crate that crates.io already holds at this version, so a
# re-run after a partial failure completes the release instead of aborting.
#
# `cargo publish` waits for each crate to appear in the index before it
# returns, so the next crate in the order resolves it.
#
# The command is identical in Bash and fish.
set -euo pipefail

# Dependency order. `mcpmem-core` first, the three crates that depend on it
# next, the binary crate last.
ORDER=(mcpmem-core mcpmem-runtime mcpmem-indexer mcpmem-webhook mcpmem)
UA='mcpmem-release (https://github.com/abankowski/mcpmem)'

dry_run=0
[[ ${1:-} == "--dry-run" ]] && dry_run=1

version="$(awk '/^\[package\]/ {inpkg = 1; next}
                /^\[/ {inpkg = 0}
                inpkg && /^version = / {gsub(/[",]/, "", $3); print $3; exit}' Cargo.toml)"
printf 'releasing version %s\n\n' "${version}"

published() {
	local body
	body="$(curl -sS -H "User-Agent: ${UA}" "https://crates.io/api/v1/crates/$1/${version}")"
	[[ ${body} == *'"num":"'"${version}"'"'* ]]
}

for crate in "${ORDER[@]}"; do
	if published "${crate}"; then
		printf 'skip     %s %s is already published\n' "${crate}" "${version}"
		continue
	fi
	if [[ ${dry_run} -eq 1 ]]; then
		printf 'dry-run  %s\n' "${crate}"
		# A dependent cannot be verified before its dependency exists on
		# crates.io, so the first release only dry-runs mcpmem-core.
		cargo publish -p "${crate}" --locked --dry-run || {
			printf 'dry-run  %s could not be verified yet\n' "${crate}"
			continue
		}
	else
		printf 'publish  %s\n' "${crate}"
		cargo publish -p "${crate}" --locked
	fi
done

printf '\ndone\n'
