#!/usr/bin/env bash
set -euo pipefail

workflows=(
  ".github/workflows/release.yml"
  ".github/workflows/build-manual.yml"
)

arm64_cross_rev="29d00c7803f221f1b3f35e561b03792368fb8339"
arm64_cross_image="ghcr.io/cross-rs/aarch64-unknown-linux-gnu@sha256:99e041b94e7d4f31477c6ddede176688562c3762ba3833b75de3316100afc39d"

grep -Fq "${arm64_cross_image}" Cross.toml \
  || {
    echo "Cross.toml must pin Linux ARM64 to the v0.1.39/v0.1.40 cross image digest" >&2
    exit 1
  }

grep -Fq 'target: x86_64-unknown-linux-gnu' ".github/workflows/release.yml" \
  && grep -Fq 'os: ubuntu-22.04' ".github/workflows/release.yml" \
  || {
    echo ".github/workflows/release.yml must keep Linux x64 on ubuntu-22.04" >&2
    exit 1
  }

grep -Fq '"platform":"linux-x64","os":"ubuntu-22.04","target":"x86_64-unknown-linux-gnu"' ".github/workflows/build-manual.yml" \
  || {
    echo ".github/workflows/build-manual.yml must keep Linux x64 on ubuntu-22.04" >&2
    exit 1
  }

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

  grep -Fq "CROSS_GIT_REV: \"${arm64_cross_rev}\"" "${workflow}" \
    || {
      echo "${workflow} must pin cross to the v0.1.39/v0.1.40 git revision" >&2
      exit 1
    }

  grep -Fq 'cargo install cross --git https://github.com/cross-rs/cross --rev "${CROSS_GIT_REV}" --locked' "${workflow}" \
    || {
      echo "${workflow} must install cross from the pinned git revision" >&2
      exit 1
    }

  grep -Fq "docker pull ${arm64_cross_image}" "${workflow}" \
    || {
      echo "${workflow} must pre-pull the pinned Linux ARM64 cross image" >&2
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
