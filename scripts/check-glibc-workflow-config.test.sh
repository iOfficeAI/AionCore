#!/usr/bin/env bash
set -euo pipefail

workflows=(
  ".github/workflows/release.yml"
  ".github/workflows/build-manual.yml"
)

for workflow in "${workflows[@]}"; do
  if [[ ! -f "${workflow}" ]]; then
    echo "Workflow not found: ${workflow}" >&2
    exit 1
  fi

  grep -Fq 'LINUX_X64_GLIBC_MAX: "GLIBC_2.34"' "${workflow}" \
    || {
      echo "${workflow} must pin the Linux x64 GLIBC ceiling to GLIBC_2.34" >&2
      exit 1
    }

  grep -Fq "matrix.target == 'x86_64-unknown-linux-gnu'" "${workflow}" \
    || {
      echo "${workflow} must verify the Linux x64 GLIBC baseline" >&2
      exit 1
    }

  grep -Fq '${LINUX_X64_GLIBC_MAX}' "${workflow}" \
    || {
      echo "${workflow} must pass LINUX_X64_GLIBC_MAX to the GLIBC checker" >&2
      exit 1
    }
done

echo "Linux GLIBC workflow config is pinned for x64 and arm64"
