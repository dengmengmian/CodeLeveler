#!/usr/bin/env bash
# Undo what an integration test does to the Xcode build, including Xcode's
# memory of it.
#
# `flutter test integration_test/…` rewrites `ios/Flutter/Generated.xcconfig` so
# FLUTTER_TARGET names a temporary listener entrypoint, and leaves it that way.
# The temp file is deleted minutes later, and every later build dies with
# "Command PhaseScriptExecution failed with a nonzero exit code" — a message
# with no hint of the cause.
#
# Repairing the config is not enough on its own: Xcode caches the environment it
# baked into the script phase, so the IDE goes on running the old command after
# the file is correct, and the build fails in Xcode while succeeding from the
# command line. Its cached build description has to go too.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

CONFIG="ios/Flutter/Generated.xcconfig"
BROKEN=0
if [[ ! -f "$CONFIG" ]] || grep -q "flutter_test_listener" "$CONFIG"; then
  BROKEN=1
fi

if [[ $BROKEN -eq 1 ]]; then
  echo "== FLUTTER_TARGET 还指着测试留下的临时文件，重建 =="
  flutter build ios --simulator --debug >/dev/null
fi

# Even when the file is already right, Xcode may still be replaying the old
# script environment; clearing its cache is harmless and is usually what makes
# the IDE agree with the command line.
CACHES=(~/Library/Developer/Xcode/DerivedData/Runner-*)
if [[ -e "${CACHES[0]}" ]]; then
  echo "== 清掉 Xcode 对 Runner 的构建缓存 =="
  rm -rf "${CACHES[@]}"
fi

echo "已修好：FLUTTER_TARGET=$(sed -n 's/^FLUTTER_TARGET=//p' "$CONFIG")"
echo "回到 Xcode 直接 Run 即可（不需要再 Clean）。"
