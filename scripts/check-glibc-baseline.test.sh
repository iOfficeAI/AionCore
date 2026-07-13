#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECKER="${ROOT_DIR}/scripts/check-glibc-baseline.sh"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

FAKE_OBJDUMP="${TMP_DIR}/objdump"
FAKE_BINARY="${TMP_DIR}/aioncore"
touch "${FAKE_BINARY}"

write_fake_objdump() {
  local glibc_version="$1"
  cat > "${FAKE_OBJDUMP}" <<EOF
#!/usr/bin/env bash
cat <<'SYMBOLS'
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.17) pthread_self
0000000000000000      DF *UND*  0000000000000000 (${glibc_version}) pidfd_spawnp
SYMBOLS
EOF
  chmod +x "${FAKE_OBJDUMP}"
}

write_fake_objdump "GLIBC_2.30"
PATH="${TMP_DIR}:${PATH}" "${CHECKER}" "${FAKE_BINARY}" "GLIBC_2.30"

write_fake_objdump "GLIBC_2.39"
if PATH="${TMP_DIR}:${PATH}" "${CHECKER}" "${FAKE_BINARY}" "GLIBC_2.30" >"${TMP_DIR}/fail.out" 2>&1; then
  echo "expected GLIBC_2.39 to fail against GLIBC_2.30 ceiling" >&2
  exit 1
fi

grep -q "exceeds GLIBC_2.30" "${TMP_DIR}/fail.out"

cat > "${FAKE_OBJDUMP}" <<'EOF'
#!/usr/bin/env bash
cat <<'SYMBOLS'
0000000000000000      DF *UND*  0000000000000000 pthread_self
SYMBOLS
EOF
chmod +x "${FAKE_OBJDUMP}"

if PATH="${TMP_DIR}:${PATH}" "${CHECKER}" "${FAKE_BINARY}" "GLIBC_2.30" >"${TMP_DIR}/empty.out" 2>&1; then
  echo "expected missing GLIBC symbols to fail" >&2
  exit 1
fi

grep -q "No GLIBC versioned symbols found" "${TMP_DIR}/empty.out"
