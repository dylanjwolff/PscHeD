#!/usr/bin/env bash
# Checks out every submodule at its pinned commit. Shallow clones keep glibc
# fast, but some hosts (repo.or.cz, which serves musl's libc-test) refuse to
# fetch an unadvertised commit shallowly once their branch has moved on, so
# fall back to full history for those. Used by CI and .agents/setup.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
git submodule sync --quiet
git submodule init

while read -r path; do
  git submodule update --depth 1 -- "$path" 2>/dev/null && continue
  echo "shallow fetch refused for $path; fetching full history"
  git -C "$path" fetch --quiet --unshallow 2>/dev/null || git -C "$path" fetch --quiet
  git submodule update -- "$path"
done < <(git config -f .gitmodules --get-regexp '\.path$' | awk '{print $2}')
