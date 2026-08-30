#!/usr/bin/env bash
set -euo pipefail

tag_ref="${1:?usage: verify_release_branch_head.sh <tag-ref> <tag-name>}"
tag_name="${2:?usage: verify_release_branch_head.sh <tag-ref> <tag-name>}"
tag_sha="$(git rev-parse "${tag_ref}^{commit}")"

if ! remote_heads="$(git ls-remote --heads origin)"; then
  echo "failed to query remote branch heads" >&2
  exit 1
fi

matching_branches=()
while IFS=$'\t' read -r branch_sha branch_ref; do
  if [[ "${branch_sha}" == "${tag_sha}" && "${branch_ref}" == refs/heads/* ]]; then
    matching_branches+=("${branch_ref#refs/heads/}")
  fi
done <<<"${remote_heads}"

if (( ${#matching_branches[@]} == 0 )); then
  echo "release tag ${tag_name} points to ${tag_sha}, which is not the HEAD of any pushed branch" >&2
  echo "push the intended release branch first, then tag that exact branch HEAD" >&2
  exit 1
fi

printf 'release tag %s points to pushed branch HEAD: %s\n' \
  "${tag_name}" "${matching_branches[*]}"
