#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: $0 <binary> <max-glibc>" >&2
  exit 2
fi

binary="$1"
max_glibc="$2"

if [[ ! -f "${binary}" ]]; then
  echo "Binary not found: ${binary}" >&2
  exit 2
fi

if [[ ! "${max_glibc}" =~ ^GLIBC_[0-9]+[.][0-9]+$ ]]; then
  echo "Invalid GLIBC ceiling: ${max_glibc}" >&2
  exit 2
fi

symbols="$(objdump -T "${binary}")"
required_glibc="$(
  printf '%s\n' "${symbols}" \
    | grep -oE 'GLIBC_[0-9]+[.][0-9]+' \
    | sort -Vu \
    | tail -1 \
    || true
)"

if [[ -z "${required_glibc}" ]]; then
  echo "No GLIBC versioned symbols found in ${binary}" >&2
  exit 2
fi

highest="$(
  printf '%s\n%s\n' "${required_glibc}" "${max_glibc}" \
    | sort -Vu \
    | tail -1
)"

if [[ "${highest}" != "${max_glibc}" ]]; then
  echo "Required GLIBC ${required_glibc} exceeds ${max_glibc} for ${binary}" >&2
  exit 1
fi

echo "Required GLIBC ${required_glibc} is within ${max_glibc} for ${binary}"
