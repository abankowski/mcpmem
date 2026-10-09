#!/usr/bin/env bash
# Merge a release bump pull request and retry once if GitHub changes its state.
#
#   scripts/merge-bump-pr.sh <head-branch>
set -euo pipefail

if [[ $# -ne 1 ]]; then
	printf 'usage: %s <head-branch>\n' "$0" >&2
	exit 2
fi

head_branch="$1"

merge_for_state() {
	local merge_state="$1"
	if [[ ${merge_state} = CLEAN ]]; then
		gh pr merge "${head_branch}" --rebase
	else
		gh pr merge "${head_branch}" --auto --rebase
	fi
}

first_state="$(gh pr view "${head_branch}" --json mergeStateStatus --jq .mergeStateStatus)"
if merge_for_state "${first_state}"; then
	exit 0
fi

second_state="$(gh pr view "${head_branch}" --json mergeStateStatus --jq .mergeStateStatus)"
if [[ ${second_state} = "${first_state}" ]]; then
	printf 'bump merge failed and merge state stayed %s; no retry\n' "${first_state}" >&2
	exit 1
fi

merge_for_state "${second_state}"
