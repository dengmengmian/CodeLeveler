#!/usr/bin/env bash
# Drive the whole remote-control chain against a booted iOS simulator.
#
# Starts a scripted model, a relay, enrols this machine as a host, runs the
# agent in a scratch repository, prints a pairing payload, hands it to the app
# running on the simulator, and accepts the pairing from the terminal while the
# app waits — which is the actual sequence a user lives, with nothing stubbed
# below the app.
#
# The model is scripted (scripted_provider.py) so the test can assert what
# should be on screen; the scratch repository means the command the test
# approves really runs and really deletes a file, without touching this one.
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
mkdir -p "$LEVELER_HOME"
export LEVELER_RELAY_ENROLLMENT_SECRET="simulator-acceptance-secret"
PORT="${PORT:-18443}"
PROVIDER_PORT="${PROVIDER_PORT:-18500}"

cleanup() {
  for pid in "${SERVE_PID:-}" "${AGENT_PID:-}" "${RELAY_PID:-}" "${PROVIDER_PID:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
  # `flutter test` rewrites ios/Flutter/Generated.xcconfig to point FLUTTER_TARGET
  # at a temporary test entrypoint, and leaves it there. That temp file is gone
  # by the time anyone opens Xcode, and the build then fails with a
  # PhaseScriptExecution error that says nothing about why. Put it back.
  if grep -q "flutter_test_listener" "$REPO/apps/leveler-mobile/ios/Flutter/Generated.xcconfig" 2>/dev/null; then
    echo "== 还原 Generated.xcconfig（集成测试改过它）=="
    (cd "$REPO/apps/leveler-mobile" && flutter build ios --simulator --debug >/dev/null 2>&1) || true
  fi
}
trap cleanup EXIT

# A repository of its own. The approval step really executes `rm`, and pointing
# that at this checkout would make a passing test destructive.
SCRATCH="$WORK/scratch-repo"
mkdir -p "$SCRATCH"
git -C "$SCRATCH" init -q .
echo "临时文件，供验收删除" > "$SCRATCH/scratch.txt"
git -C "$SCRATCH" add -A
git -C "$SCRATCH" -c user.email=acceptance@local -c user.name=acceptance commit -qm "scratch"

echo "== 脚本化模型 =="
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

echo "== relay =="
LEVELER_RELAY_BIND="127.0.0.1:$PORT" "$RELAY" > "$WORK/relay.log" 2>&1 &
RELAY_PID=$!
sleep 1

# Check it is *ours* listening. A port already taken by another relay makes the
# next step fail with a bare 401 — the enrollment secret belongs to a different
# process — and nothing about that error points at the port.
if ! kill -0 "$RELAY_PID" 2>/dev/null; then
  echo "relay 没起来（端口 $PORT 可能被占）：" >&2
  cat "$WORK/relay.log" >&2
  echo "换个端口重试： PORT=18444 $0 $DEVICE" >&2
  exit 1
fi

echo "== 启用并注册本机 =="
"$LEVELER" remote enable --relay-url "http://127.0.0.1:$PORT" --name "模拟器验收"
"$LEVELER" remote enroll

# The fingerprint the app must display for this host: it is anchored from the
# payload, so a mismatch would mean the app trusted a key the host does not hold.
HOST_FINGERPRINT="$("$LEVELER" remote status | sed -n 's/.*公钥指纹：//p')"
echo "本机指纹: $HOST_FINGERPRINT"

# A project for the phone to enter. Without one the run stops at the project
# list, which leaves the session stream — the part that needs an authorized
# WebSocket — untested; it was broken that way once already.
echo "== 打开一个项目（scratch 仓库）=="
printf '{"projects":["%s"],"aliases":{},"ignored":[]}' "$SCRATCH" > "$LEVELER_HOME/web-projects.json"
"$LEVELER" --repo "$SCRATCH" serve > "$WORK/serve.log" 2>&1 &
SERVE_PID=$!

echo "== agent =="
"$LEVELER" remote agent > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!
sleep 4

# Capture first, match after. Piping straight into `grep -q` makes grep exit on
# the first match, `leveler` die of a broken pipe, and `pipefail` report the
# whole thing as a failure — precisely when the check succeeded.
PROJECTS="$("$LEVELER" remote projects)"
if ! grep -q 在线 <<<"$PROJECTS"; then
  echo "项目没有上线，agent 会看到空列表：" >&2
  echo "$PROJECTS" >&2
  tail -5 "$WORK/serve.log" >&2
  exit 1
fi

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
      CONFIRMED="$("$LEVELER" remote confirm --yes)"
      grep -q "配对完成" <<<"$CONFIRMED" && echo "== 已在电脑上确认 =="
      exit 0
    fi
    sleep 1
  done
  echo "== 一直没有等到手机提交配对 ==" >&2
) &
CONFIRM_PID=$!

echo "== 在模拟器上跑集成测试 =="
cd "$REPO/apps/leveler-mobile"
set +e
flutter test integration_test/pairing_flow_test.dart \
  -d "$DEVICE" \
  --dart-define="PAIRING_PAYLOAD=$PAYLOAD" \
  --dart-define="HOST_FINGERPRINT=$HOST_FINGERPRINT"
RESULT=$?
set -e

wait "$CONFIRM_PID" 2>/dev/null || true

# The approval was for a real `rm`. If the file survived, the phone's "allow"
# never reached a process that could act on it — which every screen-level
# assertion would still have passed.
if [[ $RESULT -eq 0 ]]; then
  if [[ -e "$SCRATCH/scratch.txt" ]]; then
    echo "!! 批准之后 scratch.txt 还在：审批没有真正执行" >&2
    RESULT=1
  else
    echo "== 批准的命令确实执行了（scratch.txt 已删除）=="
  fi
fi

echo
echo "== 模型日志 =="
tail -8 "$WORK/provider.log"
echo "== agent 日志 =="
tail -5 "$WORK/agent.log"
exit $RESULT
