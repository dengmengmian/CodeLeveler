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
# Usage:  ./scripts/simulator_pairing.sh <device-udid>
#         (build the host binaries first: cargo build -p leveler-cli -p leveler-relay)
#
# Works against a real iPhone too, with one difference that matters: a phone is
# not this machine, so it cannot reach 127.0.0.1. Give it the address the phone
# can dial and make sure both are on the same Wi-Fi:
#
#     RELAY_HOST=192.168.1.23 ./scripts/simulator_pairing.sh <iphone-udid>
#
# `ipconfig getifaddr en0` prints that address on a Mac.
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
  [[ -x "$binary" ]] || { echo "缺少 ${binary}，先跑 cargo build -p leveler-cli -p leveler-relay" >&2; exit 1; }
done

WORK="$(mktemp -d)"
export LEVELER_HOME="$WORK/.leveler"
mkdir -p "$LEVELER_HOME"
export LEVELER_RELAY_ENROLLMENT_SECRET="simulator-acceptance-secret"
PORT="${PORT:-18443}"
PROVIDER_PORT="${PROVIDER_PORT:-18500}"
# Where the *phone* should look for the relay. Loopback is right for a
# simulator, which shares this machine's network stack, and useless for a real
# device.
RELAY_HOST="${RELAY_HOST:-127.0.0.1}"
# Bind wider than loopback when the phone is elsewhere, or nothing off this
# machine can connect however right the address is.
RELAY_BIND="127.0.0.1"
if [[ "$RELAY_HOST" != "127.0.0.1" ]]; then
  RELAY_BIND="0.0.0.0"
fi

cleanup() {
  # Printed here rather than at the end of the happy path: the logs are most
  # wanted exactly when the run did not get there.
  if [[ -n "${WORK:-}" ]]; then
    echo
    echo "== 模型日志 =="
    tail -12 "$WORK/provider.log" 2>/dev/null || true
    echo "== agent 日志 =="
    tail -6 "$WORK/agent.log" 2>/dev/null || true
    echo "== 项目 A daemon =="
    tail -6 "$WORK/serve.log" 2>/dev/null || true
    # Per-frame record of what the agent admitted or refused: when a phone's
    # message seems to vanish, this is the only place that says which.
    echo "== 远程审计 =="
    tail -12 "$LEVELER_HOME"/remote/audit/*.jsonl 2>/dev/null || true
  fi
  for pid in "${SERVE_PID:-}" "${SERVE_B_PID:-}" "${AGENT_PID:-}" "${RELAY_PID:-}" "${PROVIDER_PID:-}"; do
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

# Two repositories of their own. Two, because the acceptance list asks that
# switching projects on the phone not cross wires, and one project cannot show
# that. Their own, because the approval step really executes `rm`, and pointing
# that at this checkout would make a passing test destructive.
SCRATCH="$WORK/scratch-alpha"
SCRATCH_B="$WORK/scratch-beta"
for repo in "$SCRATCH" "$SCRATCH_B"; do
  mkdir -p "$repo"
  git -C "$repo" init -q .
  echo "临时文件，供验收删除" > "$repo/scratch.txt"
  git -C "$repo" add -A
  git -C "$repo" -c user.email=acceptance@local -c user.name=acceptance commit -qm "scratch"
done

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
LEVELER_RELAY_BIND="$RELAY_BIND:$PORT" "$RELAY" > "$WORK/relay.log" 2>&1 &
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
"$LEVELER" remote enable --relay-url "http://$RELAY_HOST:$PORT" --name "验收"
"$LEVELER" remote enroll

# The fingerprint the app must display for this host: it is anchored from the
# payload, so a mismatch would mean the app trusted a key the host does not hold.
HOST_FINGERPRINT="$("$LEVELER" remote status | sed -n 's/.*公钥指纹：//p')"
echo "本机指纹: $HOST_FINGERPRINT"

# Projects for the phone to enter. Without one the run stops at the project
# list, which leaves the session stream — the part that needs an authorized
# WebSocket — untested; it was broken that way once already.
echo "== 打开两个项目 =="
printf '{"projects":["%s","%s"],"aliases":{},"ignored":[]}' "$SCRATCH" "$SCRATCH_B" \
  > "$LEVELER_HOME/web-projects.json"
"$LEVELER" --repo "$SCRATCH" serve > "$WORK/serve.log" 2>&1 &
SERVE_PID=$!
"$LEVELER" --repo "$SCRATCH_B" serve > "$WORK/serve-b.log" 2>&1 &
SERVE_B_PID=$!

echo "== agent =="
"$LEVELER" remote agent > "$WORK/agent.log" 2>&1 &
AGENT_PID=$!
sleep 4

# Capture first, match after. Piping straight into `grep -q` makes grep exit on
# the first match, `leveler` die of a broken pipe, and `pipefail` report the
# whole thing as a failure — precisely when the check succeeded.
PROJECTS="$("$LEVELER" remote projects)"
if [[ "$(grep -c 在线 <<<"$PROJECTS")" -lt 2 ]]; then
  echo "两个项目没有都上线，切换用例会跳过：" >&2
  echo "$PROJECTS" >&2
  tail -5 "$WORK/serve.log" "$WORK/serve-b.log" >&2
  exit 1
fi

# Accept from the terminal as soon as the app has claimed a secret.
#
# One watcher per pairing, because each `flutter test` run starts from an
# unpaired app: the store is in memory, so the second journey pairs again from
# scratch rather than inheriting the first one's trust.
# Revoke the device that was just accepted, after a pause.
#
# "Just accepted" is the last entry the store lists: `accept` appends, so the
# newest pairing is at the end. Tying this to the confirmation rather than
# diffing device lists keeps it to one fact — each journey pairs once, and this
# is that one.
revoke_newest_after() {
  local pause="$1"
  sleep "$pause"
  local newest
  newest="$("$LEVELER" remote devices 2>/dev/null |
    sed -n 's/.*(\(dev_[A-Za-z0-9_.:-]*\)).*/\1/p' | sed -n '$p')"
  if [[ -z "$newest" ]]; then
    echo "== 没有已配对设备可撤销 ==" >&2
    return 1
  fi
  echo "== 撤销刚配对的设备 $newest =="
  "$LEVELER" remote revoke "$newest" || true
}

watch_for_pairing() {
  # Wait for the phone to claim the secret, then pause before accepting. The
  # pause is measured from the claim, not from this script's start, so the
  # window in which the phone is claimed-but-not-accepted really exists — that
  # is the window the test uses to prove a device cannot promote itself.
  for _ in $(seq 1 120); do
    if "$LEVELER" remote pending >/dev/null 2>&1; then
      # Comfortably longer than the app spends checking that it is *not*
      # yet paired (four ~2s pumps). At 8 seconds the two ended in a photo
      # finish and the app occasionally saw itself paired inside its own
      # guard window — a real race in the test, not in the product.
      echo "== 手机已提交，15 秒后再确认 =="
      sleep 15
      CONFIRMED="$("$LEVELER" remote confirm --yes)"
      grep -q "配对完成" <<<"$CONFIRMED" && echo "== 已在电脑上确认 =="
      # Some journeys need the pairing taken away again while the app watches.
      if [[ -n "${REVOKE_AFTER:-}" ]]; then
        revoke_newest_after "$REVOKE_AFTER"
      fi
      return 0
    fi
    sleep 1
  done
  echo "== 一直没有等到手机提交配对 ==" >&2
  return 1
}

run_journey() {
  local journey="$1"
  echo "== 配对载荷（${journey}）=="
  PAYLOAD="$("$LEVELER" remote pair | grep '^{')"
  echo "$PAYLOAD"

  watch_for_pairing &
  CONFIRM_PID=$!

  echo "== 在模拟器上跑 ${journey} =="
  set +e
  flutter test "integration_test/${journey}.dart" \
    -d "$DEVICE" \
    --dart-define="PAIRING_PAYLOAD=$PAYLOAD" \
    --dart-define="HOST_FINGERPRINT=$HOST_FINGERPRINT"
  local step=$?
  set -e
  wait "$CONFIRM_PID" 2>/dev/null || true
  return $step
}

cd "$REPO/apps/leveler-mobile"
RESULT=0
for journey in pairing_flow_test multi_project_test; do
  set +e
  run_journey "$journey"
  STEP=$?
  set -e
  if [[ $STEP -ne 0 ]]; then
    RESULT=$STEP
    break
  fi
done

# The last journey needs the far end to go away: one project's daemon stopped
# before it starts, and this device revoked while it is watching.
if [[ $RESULT -eq 0 ]]; then
  echo "== 停掉第二个项目的 daemon =="
  kill "$SERVE_B_PID" 2>/dev/null || true
  SERVE_B_PID=""
  sleep 3

  # Revoke this journey's device 25 seconds after the host accepts it — past
  # the point where the app has reached the project list and started looking.
  set +e
  REVOKE_AFTER=25 run_journey offline_and_revoke_test
  RESULT=$?
  set -e
fi

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

exit $RESULT
