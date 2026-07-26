#!/usr/bin/env bash
# Drive the whole remote-control chain against a booted iOS simulator.
#
# Starts a relay, enrols this machine as a host, runs the agent, prints a
# pairing payload, hands it to the app running on the simulator, and accepts the
# pairing from the terminal while the app waits — which is the actual sequence a
# user lives, with nothing stubbed below the app.
#
# Usage:  ./scripts/simulator_pairing.sh <simulator-udid>
#         (build the host binaries first: cargo build -p leveler-cli -p leveler-relay)
set -euo pipefail

DEVICE="${1:-}"
if [[ -z "$DEVICE" ]]; then
  echo "用法: $0 <simulator-udid>   （xcrun simctl list devices booted）" >&2
  exit 2
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
LEVELER="$REPO/target/debug/leveler"
RELAY="$REPO/target/debug/leveler-relay"
for binary in "$LEVELER" "$RELAY"; do
  [[ -x "$binary" ]] || { echo "缺少 $binary，先跑 cargo build -p leveler-cli -p leveler-relay" >&2; exit 1; }
done

WORK="$(mktemp -d)"
export LEVELER_HOME="$WORK/.leveler"
export LEVELER_RELAY_ENROLLMENT_SECRET="simulator-acceptance-secret"
PORT="${PORT:-18443}"

cleanup() {
  [[ -n "${AGENT_PID:-}" ]] && kill "$AGENT_PID" 2>/dev/null || true
  [[ -n "${RELAY_PID:-}" ]] && kill "$RELAY_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== relay =="
LEVELER_RELAY_BIND="127.0.0.1:$PORT" "$RELAY" > "$WORK/relay.log" 2>&1 &
RELAY_PID=$!
sleep 1

echo "== 启用并注册本机 =="
"$LEVELER" remote enable --relay-url "http://127.0.0.1:$PORT" --name "模拟器验收"
"$LEVELER" remote enroll

# The fingerprint the app must display for this host: it is anchored from the
# payload, so a mismatch would mean the app trusted a key the host does not hold.
HOST_FINGERPRINT="$("$LEVELER" remote status | sed -n 's/.*公钥指纹：//p')"
echo "本机指纹: $HOST_FINGERPRINT"

echo "== agent =="
"$LEVELER" remote agent > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!
sleep 2

echo "== 配对载荷 =="
PAYLOAD="$("$LEVELER" remote pair | grep '^{')"
echo "$PAYLOAD"

# Accept from the terminal as soon as the app has claimed the secret. The app is
# waiting for exactly this, so it runs in parallel with the test.
(
  # Wait for the phone to claim the secret, then pause before accepting. The
  # pause is measured from the claim, not from this script's start, so the
  # window in which the phone is claimed-but-not-accepted really exists — that
  # is the window the test uses to prove a device cannot promote itself.
  for _ in $(seq 1 90); do
    if "$LEVELER" remote pending >/dev/null 2>&1; then
      echo "== 手机已提交，8 秒后再确认 =="
      sleep 8
      "$LEVELER" remote confirm --yes | grep -q "配对完成" && echo "== 已在电脑上确认 =="
      exit 0
    fi
    sleep 1
  done
  echo "== 一直没有等到手机提交配对 ==" >&2
) &
CONFIRM_PID=$!

echo "== 在模拟器上跑集成测试 =="
cd "$REPO/apps/leveler-mobile"
flutter test integration_test/pairing_flow_test.dart \
  -d "$DEVICE" \
  --dart-define="PAIRING_PAYLOAD=$PAYLOAD" \
  --dart-define="HOST_FINGERPRINT=$HOST_FINGERPRINT"
RESULT=$?

wait "$CONFIRM_PID" 2>/dev/null || true
echo
echo "== agent 日志 =="
tail -5 "$WORK/agent.log"
exit $RESULT
