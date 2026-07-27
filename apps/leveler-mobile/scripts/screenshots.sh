#!/usr/bin/env bash
# Photograph every screen of the app against a real host.
#
# Same host setup as `simulator_pairing.sh`, but the journey is
# `screenshots_test.dart`, which pauses on each screen instead of asserting.
# A snapper takes one frame a second and names it after whichever screen the
# test last announced, so the output is a directory of labelled pictures rather
# than a pile of timestamps.
#
# Usage:  ./scripts/screenshots.sh <simulator-udid> [output-dir]
set -euo pipefail

DEVICE="${1:-}"
OUT="${2:-/tmp/leveler-shots}"
if [[ -z "$DEVICE" ]]; then
  echo "用法: $0 <simulator-udid> [输出目录]" >&2
  exit 2
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
LEVELER="$REPO/target/debug/leveler"
RELAY="$REPO/target/debug/leveler-relay"
for binary in "$LEVELER" "$RELAY"; do
  [[ -x "$binary" ]] || { echo "缺少 ${binary}，先跑 cargo build -p leveler-cli -p leveler-relay" >&2; exit 1; }
done

rm -rf "$OUT"
mkdir -p "$OUT"

WORK="$(mktemp -d)"
export LEVELER_HOME="$WORK/.leveler"
mkdir -p "$LEVELER_HOME"
XCCONFIG_BACKUP="$WORK/Generated.xcconfig.orig"
cp "$REPO/apps/leveler-mobile/ios/Flutter/Generated.xcconfig" "$XCCONFIG_BACKUP" 2>/dev/null || true
export LEVELER_RELAY_ENROLLMENT_SECRET="screenshot-secret"
PORT="${PORT:-18443}"
PROVIDER_PORT="${PROVIDER_PORT:-18500}"

cleanup() {
  for pid in "${SNAP_PID:-}" "${SERVE_PID:-}" "${AGENT_PID:-}" "${RELAY_PID:-}" "${PROVIDER_PID:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
  # See simulator_pairing.sh: an integration test leaves FLUTTER_TARGET pointing
  # at a deleted temp file, and Xcode then fails with a message about nothing.
  if [[ -f "${XCCONFIG_BACKUP:-}" ]]; then
    cp "$XCCONFIG_BACKUP" "$REPO/apps/leveler-mobile/ios/Flutter/Generated.xcconfig"
  fi
  echo "截图在 $OUT"
}
trap cleanup EXIT

SCRATCH="$WORK/scratch-repo"
mkdir -p "$SCRATCH"
git -C "$SCRATCH" init -q .
echo "临时文件" > "$SCRATCH/scratch.txt"
git -C "$SCRATCH" add -A
git -C "$SCRATCH" -c user.email=shots@local -c user.name=shots commit -qm scratch

python3 "$REPO/apps/leveler-mobile/scripts/scripted_provider.py" "$PROVIDER_PORT" > "$WORK/provider.log" 2>&1 &
PROVIDER_PID=$!
cat > "$LEVELER_HOME/config.toml" <<EOF
default_model = "scripted"
lang = "zh"

[providers.scripted]
base_url = "http://127.0.0.1:$PROVIDER_PORT"
api_key = "not-used"

[models.scripted]
provider = "scripted"
model_id = "scripted"
context_window = 100000
max_output_tokens = 4096
streaming = true
tool_calling = true
EOF

LEVELER_RELAY_BIND="127.0.0.1:$PORT" "$RELAY" > "$WORK/relay.log" 2>&1 &
RELAY_PID=$!
sleep 1
"$LEVELER" remote enable --relay-url "http://127.0.0.1:$PORT" --name "截图" >/dev/null
"$LEVELER" remote enroll >/dev/null
HOST_FINGERPRINT="$("$LEVELER" remote status | sed -n 's/.*公钥指纹：//p')"

printf '{"projects":["%s"],"aliases":{},"ignored":[]}' "$SCRATCH" > "$LEVELER_HOME/web-projects.json"
"$LEVELER" --repo "$SCRATCH" serve > "$WORK/serve.log" 2>&1 &
SERVE_PID=$!
"$LEVELER" remote agent > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!
sleep 4

PAYLOAD="$("$LEVELER" remote pair | grep '^{')"

(
  for _ in $(seq 1 120); do
    if "$LEVELER" remote pending >/dev/null 2>&1; then
      sleep 15
      "$LEVELER" remote confirm --yes >/dev/null
      exit 0
    fi
    sleep 1
  done
) &

# One numbered frame a second.
#
# Naming them after the screen the test announces sounds better and does not
# work: `flutter test`'s output reaches this script in bursts, so several
# screens' labels can arrive within one second and all but the last are never
# sampled. A plain counter cannot be wrong, and the pictures are in order.
(
  count=0
  while true; do
    count=$((count + 1))
    xcrun simctl io "$DEVICE" screenshot --type=png \
      "$OUT/$(printf '%03d' "$count").png" >/dev/null 2>&1 || true
    sleep 1
  done
) &
SNAP_PID=$!

cd "$REPO/apps/leveler-mobile"
set +e
flutter test integration_test/screenshots_test.dart \
  -d "$DEVICE" \
  --dart-define="PAIRING_PAYLOAD=$PAYLOAD" \
  --dart-define="HOST_FINGERPRINT=$HOST_FINGERPRINT" 2>&1 |
  while IFS= read -r line; do
    # Stamped, so a screen announcement can be lined up with the frame numbers
    # afterwards even though the output arrives in bursts.
    printf '[%s] %s\n' "$(date +%s)" "$line"
  done
RESULT=${PIPESTATUS[0]}
set -e
exit $RESULT
