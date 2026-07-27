#!/usr/bin/env bash
# Point Xcode back at the app after an integration test ran.
#
# `flutter test integration_test/…` rewrites `ios/Flutter/Generated.xcconfig` so
# FLUTTER_TARGET names a temporary listener entrypoint, and leaves it that way.
# The temp file is gone minutes later, so the next Xcode build fails with
# "Command PhaseScriptExecution failed with a nonzero exit code" — a message
# that says nothing about the cause.
#
# The acceptance scripts repair this themselves on exit; this is for when one
# was interrupted, or when the build was run some other way.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

CONFIG="ios/Flutter/Generated.xcconfig"
if [[ -f "$CONFIG" ]] && ! grep -q "flutter_test_listener" "$CONFIG"; then
  echo "没问题：FLUTTER_TARGET 指向 $(sed -n 's/^FLUTTER_TARGET=//p' "$CONFIG")"
  exit 0
fi

echo "== FLUTTER_TARGET 还指着测试留下的临时文件，重建 =="
flutter build ios --simulator --debug >/dev/null
echo "已修好：$(sed -n 's/^FLUTTER_TARGET=//p' "$CONFIG")"
