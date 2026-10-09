#!/usr/bin/env bash
# Tests for scripts/merge-bump-pr.sh.
#
#   scripts/test-merge-bump-pr.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BRANCH='chore/open-1.2.4'
failures=0

fail() {
	printf 'FAIL  %s\n' "$1" >&2
	failures=$((failures + 1))
}

expect() {
	local name="$1" want_status="$2" states="$3" merge_results="$4" want_merges="$5" want_output="$6"
	local temp output status got_merges
	temp="$(mktemp -d)"
	cat >"${temp}/gh" <<'FAKE_GH'
#!/usr/bin/env bash
set -euo pipefail

case "$1 $2" in
'pr view')
	read -r -a states <<<"${FAKE_GH_STATES}"
	view_index="$(cat "${FAKE_GH_STATE_INDEX}")"
	printf '%s' "$((view_index + 1))" >"${FAKE_GH_STATE_INDEX}"
	printf '%s\n' "${states[view_index]}"
	;;
'pr merge')
	printf '%s\n' "$*" >>"${FAKE_GH_LOG}"
	read -r -a merge_results <<<"${FAKE_GH_MERGE_RESULTS}"
	merge_index="$(wc -l <"${FAKE_GH_LOG}")"
	merge_index=$((merge_index - 1))
	if [[ ${merge_results[merge_index]} = fail ]]; then
		printf 'fake merge %d failed\n' "$((merge_index + 1))" >&2
		exit 1
	fi
	;;
*)
	printf 'unexpected gh command: %s\n' "$*" >&2
	exit 2
	;;
esac
FAKE_GH
	chmod +x "${temp}/gh"
	printf '0' >"${temp}/state-index"

	if output="$(PATH="${temp}:${PATH}" FAKE_GH_STATES="${states}" FAKE_GH_MERGE_RESULTS="${merge_results}" FAKE_GH_LOG="${temp}/merges" FAKE_GH_STATE_INDEX="${temp}/state-index" "${HERE}/merge-bump-pr.sh" "${BRANCH}" 2>&1)"; then
		status=0
	else
		status=$?
	fi
	got_merges="$(cat "${temp}/merges" 2>/dev/null || true)"

	if [[ ${status} -ne ${want_status} ]]; then
		fail "${name}: exit ${status}, want ${want_status}; output: ${output}"
	elif [[ ${got_merges} != "${want_merges}" ]]; then
		fail "${name}: merge requests were ${got_merges}, want ${want_merges}"
	elif [[ ${output} != *"${want_output}"* ]]; then
		fail "${name}: output was ${output}, want ${want_output}"
	else
		printf 'PASS  %s\n' "${name}"
	fi
	rm -rf "${temp}"
}

expect 'clean state merges directly' 0 \
	'CLEAN' 'success' \
	$'pr merge chore/open-1.2.4 --rebase' ''
expect 'changed state retries once' 0 \
	'UNKNOWN CLEAN' 'fail success' \
	$'pr merge chore/open-1.2.4 --auto --rebase\npr merge chore/open-1.2.4 --rebase' \
	'fake merge 1 failed'
expect 'unchanged state does not retry' 1 \
	'UNKNOWN UNKNOWN' 'fail success' \
	$'pr merge chore/open-1.2.4 --auto --rebase' \
	'fake merge 1 failed'
expect 'failed retry returns failure' 1 \
	'UNKNOWN CLEAN' 'fail fail' \
	$'pr merge chore/open-1.2.4 --auto --rebase\npr merge chore/open-1.2.4 --rebase' \
	'fake merge 2 failed'

printf '\nFAILURES: %d\n' "${failures}"
[[ ${failures} -eq 0 ]]
