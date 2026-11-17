#!/bin/sh
# End-to-end fail-closed tests for install.sh using a fully local fake release.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
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
    # A pinned install must never ask for 'latest' — GitHub answers that with
    # the newest STABLE release, which is exactly what a beta is not.
    if [ "${INSTALL_TEST_MODE:-}" = pinned ]; then
      exit 22
    fi
    printf '%s\n' '{"tag_name":"v1.2.3"}'
    ;;
  *.tar.xz.sha256)
    case "$INSTALL_TEST_MODE" in
      gz-only) exit 22 ;;
      missing) exit 22 ;;
      invalid) printf '%s\n' 'not-a-sha256' >"$out" ;;
      *) printf '%s  %s\n' \
        'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
        'leveler-v1.2.3-x86_64-unknown-linux-gnu.tar.xz' >"$out" ;;
    esac
    ;;
  *.tar.gz.sha256)
    printf '%s  %s\n' \
      'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
      'leveler-v1.2.3-x86_64-unknown-linux-gnu.tar.gz' >"$out"
    ;;
  *.tar.xz)
    if [ "${INSTALL_TEST_MODE:-}" = gz-only ]; then
      exit 22
    fi
    : >"$out"
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
  pin="${3:-}"
  HOME="$fixture/home" \
    LEVELER_BIN_DIR="$fixture/install" \
    INSTALL_TEST_MODE="$mode" \
    LEVELER_VERSION="$pin" \
    PATH="$fixture/bin" \
    /bin/sh "$ROOT/install.sh"
}

# `LEVELER_VERSION` installs exactly the named tag and asks nothing about
# 'latest'. This is the only route to a pre-release, so it has to work with the
# latest endpoint refusing to answer.
expect_pinned_success() {
  name="$1"
  pin="$2"
  fixture="$TEST_ROOT/$name"
  make_fixture "$fixture"
  add_hash_tool "$fixture"
  run_installer "$fixture" pinned "$pin" >/dev/null
  [ -x "$fixture/install/leveler" ] || {
    printf 'FAIL: %s did not install the pinned version\n' "$name" >&2
    exit 1
  }
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

expect_pinned_success pinned-with-v v1.2.3
expect_pinned_success pinned-without-v 1.2.3

# `releases/latest` still names a release published before the xz switch:
# only the gzip sibling exists, and the one-line installer must not 404.
gz_only="$TEST_ROOT/gz-only"
make_fixture "$gz_only"
add_hash_tool "$gz_only"
run_installer "$gz_only" gz-only >/dev/null
[ -x "$gz_only/install/leveler" ] || {
  printf 'FAIL: the installer did not fall back to the gzip archive\n' >&2
  exit 1
}

printf 'install.sh checksum tests passed\n'
