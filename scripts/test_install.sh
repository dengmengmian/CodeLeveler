#!/bin/sh
# End-to-end fail-closed tests for install.sh using a fully local fake release.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

make_fixture() {
  fixture="$1"
  mkdir -p "$fixture/bin" "$fixture/home" "$fixture/install"
  for tool in grep head cut mktemp rm mkdir mv chmod tr; do
    ln -s "$(command -v "$tool")" "$fixture/bin/$tool"
  done

  cat >"$fixture/bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) printf '%s\n' Linux ;;
esac
EOF

  cat >"$fixture/bin/curl" <<'EOF'
#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) shift; out="$1" ;;
    http://*|https://*) url="$1" ;;
  esac
  shift
done
case "$url" in
  */releases/latest)
    printf '%s\n' '{"tag_name":"v1.2.3"}'
    ;;
  *.tar.gz.sha256)
    case "$INSTALL_TEST_MODE" in
      missing) exit 22 ;;
      invalid) printf '%s\n' 'not-a-sha256' >"$out" ;;
      *) printf '%s  %s\n' \
        'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
        'leveler-v1.2.3-x86_64-unknown-linux-gnu.tar.gz' >"$out" ;;
    esac
    ;;
  *.tar.gz)
    : >"$out"
    ;;
  *) exit 22 ;;
esac
EOF

  cat >"$fixture/bin/tar" <<'EOF'
#!/bin/sh
destination=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -C) shift; destination="$1" ;;
  esac
  shift
done
mkdir -p "$destination/leveler-v1.2.3-x86_64-unknown-linux-gnu"
printf '%s\n' '#!/bin/sh' 'exit 0' \
  >"$destination/leveler-v1.2.3-x86_64-unknown-linux-gnu/leveler"
EOF

  chmod +x "$fixture/bin/uname" "$fixture/bin/curl" "$fixture/bin/tar"
}

add_hash_tool() {
  fixture="$1"
  cat >"$fixture/bin/shasum" <<'EOF'
#!/bin/sh
case "$INSTALL_TEST_MODE" in
  mismatch) hash='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' ;;
  invalid-tool) hash='not-a-hash' ;;
  *) hash='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
esac
printf '%s  %s\n' "$hash" "$3"
EOF
  chmod +x "$fixture/bin/shasum"
}

run_installer() {
  fixture="$1"
  mode="$2"
  HOME="$fixture/home" \
    LEVELER_BIN_DIR="$fixture/install" \
    INSTALL_TEST_MODE="$mode" \
    PATH="$fixture/bin" \
    /bin/sh "$ROOT/install.sh"
}

expect_failure() {
  name="$1"
  mode="$2"
  expected="$3"
  with_hash_tool="$4"
  fixture="$TEST_ROOT/$name"
  make_fixture "$fixture"
  if [ "$with_hash_tool" = yes ]; then
    add_hash_tool "$fixture"
  fi
  if output="$(run_installer "$fixture" "$mode" 2>&1)"; then
    printf 'FAIL: %s unexpectedly succeeded\n' "$name" >&2
    exit 1
  fi
  case "$output" in
    *"$expected"*) : ;;
    *) printf 'FAIL: %s output did not contain %s:\n%s\n' "$name" "$expected" "$output" >&2; exit 1 ;;
  esac
  [ ! -e "$fixture/install/leveler" ] || {
    printf 'FAIL: %s installed an unverified binary\n' "$name" >&2
    exit 1
  }
}

expect_failure missing-checksum missing "checksum download failed" yes
expect_failure invalid-checksum invalid "invalid sha256 checksum file" yes
expect_failure missing-hash-tool valid "shasum or sha256sum is required" no
expect_failure checksum-mismatch mismatch "sha256 mismatch" yes
expect_failure invalid-hash-output invalid-tool "hash tool returned an invalid sha256" yes

success="$TEST_ROOT/success"
make_fixture "$success"
add_hash_tool "$success"
run_installer "$success" valid >/dev/null
[ -x "$success/install/leveler" ] || {
  printf 'FAIL: verified binary was not installed\n' >&2
  exit 1
}

printf 'install.sh checksum tests passed\n'
