#!/usr/bin/env bash
# Publish the workspace to crates.io in dependency order.
#
#   scripts/publish-crates.sh              # publish
#   scripts/publish-crates.sh --dry-run    # package and verify only
#
# The script skips a crate that crates.io already holds at this version, so a
# re-run after a partial failure completes the release instead of aborting.
#
# Cargo index resolution can lag crates.io API visibility. Before each real
# publish, retry a Cargo dry run until Cargo resolves the crate dependencies.
#
# The command is identical in Bash and fish.
set -euo pipefail

# Dependency order. `mcpmem-core` first, the crates that depend on it next,
# the binary crate last. `mcpmem-oauth` must precede `mcpmem`: the binary
# depends on it with a version requirement, and the release on 2026-09-11
# failed exactly there, because the order skipped it while the manifest had
# required it since the oauth work landed.
ORDER=(mcpmem-core mcpmem-extractor mcpmem-runtime mcpmem-indexer mcpmem-webhook mcpmem-oauth mcpmem)
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

wait_until_resolvable() {
	local crate="$1"
	shift
	local -a publish_args=("$@")
	local attempt
	local max_attempts=12

	for ((attempt = 1; attempt <= max_attempts; attempt++)); do
		if cargo publish -p "${crate}" "${publish_args[@]}" --dry-run; then
			return 0
		fi
		if [[ ${attempt} -lt ${max_attempts} ]]; then
			printf 'wait     %s Cargo cannot resolve dependencies (%d/%d)\n' "${crate}" "${attempt}" "${max_attempts}"
			sleep 5
		fi
	done

	printf 'error: %s Cargo could not resolve dependencies after %d dry-run attempts\n' "${crate}" "${max_attempts}" >&2
	return 1
}

for crate in "${ORDER[@]}"; do
	if published "${crate}"; then
		printf 'skip     %s %s is already published\n' "${crate}" "${version}"
		continue
	fi
	publish_args=(--locked)
	if [[ ${crate} == "mcpmem" ]]; then
		# The root `mcpmem` package owns the built UI bundle: its `include` list walks
		# the tree, so the git-ignored ui/dist restored from the frontend
		# artifact lands in the package and cargo's dirty check refuses it.
		# The release on 2026-10-05 (v3.0.0) published the six library
		# crates and failed here; the uncommitted files are the bundle the
		# crate must embed, not contamination. Keep root verification because
		# this package changes when it carries ui/dist.
		publish_args+=(--allow-dirty)
	else
		# CI proves the exact tag, and the archive matrix builds every target
		# before publishing. Library packages do not carry the generated bundle.
		publish_args+=(--no-verify)
	fi
	if [[ ${dry_run} -eq 1 ]]; then
		printf 'dry-run  %s\n' "${crate}"
		# A dependent cannot be verified before its dependency exists on
		# crates.io, so the first release only dry-runs mcpmem-core.
		cargo publish -p "${crate}" "${publish_args[@]}" --dry-run || {
			printf 'dry-run  %s could not be verified yet\n' "${crate}"
			continue
		}
	else
		printf 'verify   %s\n' "${crate}"
		wait_until_resolvable "${crate}" "${publish_args[@]}"
		printf 'publish  %s\n' "${crate}"
		cargo publish -p "${crate}" "${publish_args[@]}"
	fi
done

printf '\ndone\n'
