#!/usr/bin/env bash
# Refuse an `include_str!` or `include_bytes!` that reads outside its own crate.
#
#   scripts/check-crate-includes.sh
#
# `cargo package` copies only the files under a crate root, so a macro that
# escapes that root compiles in the workspace and fails while verifying the
# tarball. The v1.0.0-rc.1 release failed exactly there:
# crates/mcpmem-core/src/events.rs read ../../../migrations/*.sql, which the
# packaged crate did not contain.
#
# The command is identical in Bash and fish.
set -uo pipefail

failures=0

# Crate roots: the workspace root package plus every member under crates/.
roots=(. )
for member in crates/*/; do roots+=("${member%/}"); done

# Every directory `cargo package` copies and then compiles. `src` alone was not
# enough: a test under `tests/` read a migration that had moved into another
# crate, and the first version of this gate reported a clean workspace.
for root in "${roots[@]}"; do
	for area in src tests benches examples; do
	src="${root}/${area}"
	[[ -d ${src} ]] || continue
	while IFS= read -r hit; do
		file="${hit%%:*}"
		rest="${hit#*:}"
		line="${rest%%:*}"
		# `\|` is GNU sed only, and BSD sed silently matched nothing here, which
		# made the whole gate pass while three includes escaped their crate.
		# Keep both patterns POSIX so the same script decides the same way on a
		# laptop and on a runner.
		target="$(printf '%s' "${hit}" | sed -n 's/.*include_[a-z]*!("\([^"]*\)".*/\1/p')"
		if [[ -z ${target} ]]; then
			printf 'FAIL  %s:%s has an include this gate cannot parse\n' "${file}" "${line}" >&2
			failures=$((failures + 1))
			continue
		fi
		# Resolve the include against the directory of the file that holds it.
		resolved="$(cd "$(dirname "${file}")" && cd "$(dirname "${target}")" 2>/dev/null && pwd)/$(basename "${target}")"
		crate_root="$(cd "${root}" && pwd)"
		if [[ ${resolved} != "${crate_root}"/* ]]; then
			printf 'FAIL  %s:%s reads outside its crate: %s\n' "${file}" "${line}" "${target}" >&2
			failures=$((failures + 1))
		fi
	done < <(grep -rnE 'include_(str|bytes)!\("' "${src}" 2>/dev/null)
	done
done

if [[ ${failures} -gt 0 ]]; then
	printf '\n%d include(s) escape their crate root; cargo package would ship a crate that cannot compile\n' "${failures}" >&2
	exit 1
fi
printf 'ok    every include_str!/include_bytes! stays inside its crate\n'
