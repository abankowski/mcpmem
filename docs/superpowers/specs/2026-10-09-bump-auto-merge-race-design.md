# Release Bump Auto-Merge Race — Design

**Status:** Approved on 2026-10-09.

Extract the state-dependent bump merge from `release.yml` into `scripts/merge-bump-pr.sh <head-branch>`. The helper reads `mergeStateStatus`, requests direct `--rebase` merge for `CLEAN`, and requests `--auto --rebase` for any other state.

If the request fails, read state once more. Retry once only if the state changed. Preserve output and fail when the state is unchanged or the retry fails. Test with a fake `gh` command for clean success, `UNKNOWN` to `CLEAN`, unchanged failure, and retry failure. Keep existing GitHub token permissions.