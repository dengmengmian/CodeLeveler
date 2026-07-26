/// The pairing flow, driven through the real UI on a real simulator against a
/// real relay and a real agent.
///
/// Everything below the app is genuine: `scripts/simulator_pairing.sh` starts
/// `leveler-relay`, enrolls a host, runs `leveler remote agent`, and accepts the
/// pairing from the terminal while this test is waiting for it. What that buys
/// over the unit tests is the parts only a device has — the keychain, the HTTP
/// stack, the platform's TLS-less loopback — and the parts only a human flow
/// has: that the fingerprint a user is asked to compare actually appears, and
/// that the app waits for an accept it cannot give itself.
///
/// Run it through the script, not directly; it needs a live host.
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/main.dart';

/// Supplied by the script from `leveler remote pair`.
const String pairingPayload = String.fromEnvironment('PAIRING_PAYLOAD');

/// What the host prints for its own key, so the test can check the app shows
/// the same machine rather than merely showing *a* fingerprint.
const String expectedHostFingerprint = String.fromEnvironment('HOST_FINGERPRINT');

/// Everything the screen is currently showing, so a failed expectation says
/// what the app did instead of only what it did not do.
String _visibleText(WidgetTester tester) => tester
    .widgetList<Text>(find.byType(Text))
    .map((text) => text.data ?? '')
    .where((text) => text.isNotEmpty)
    .join(' | ');

/// Scroll a list item into view before touching it.
///
/// A `ListView` only builds what its viewport needs, so anything below the fold
/// is absent from the tree rather than merely invisible — and `find` reports it
/// as missing, which reads like a bug in the app instead of one in the test.
///
/// Dragging by hand rather than `scrollUntilVisible`, which throws its own
/// obscure error when the target has not been built yet — the exact case this
/// helper exists to handle.
Future<void> _bringIntoView(WidgetTester tester, Finder target) async {
  for (var attempt = 0; attempt < 12; attempt++) {
    if (target.evaluate().isNotEmpty) {
      await tester.ensureVisible(target);
      await tester.pumpAndSettle();
      return;
    }
    await tester.drag(find.byType(ListView), const Offset(0, -220));
    await tester.pumpAndSettle();
  }
}

/// Scroll a button into view and press it, failing loudly if the press lands on
/// something else — a tap that silently misses is the difference between a test
/// that checks the app and one that checks nothing.
Future<void> _tapByText(WidgetTester tester, String label, {bool settle = true}) async {
  final target = find.text(label);
  await _bringIntoView(tester, target);
  expect(target, findsOneWidget, reason: '找不到「$label」，屏幕上是：${_visibleText(tester)}');
  await tester.tap(target, warnIfMissed: true);
  // `settle: false` for taps that start something the *host* finishes:
  // pumpAndSettle runs real frames until the UI stops moving, which here means
  // waiting out the very window the test is trying to observe.
  if (settle) {
    await tester.pumpAndSettle();
  } else {
    await tester.pump();
  }
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('a phone pairs with a host and reaches its projects', (tester) async {
    expect(pairingPayload, isNotEmpty,
        reason: 'run this through scripts/simulator_pairing.sh');

    // An in-memory vault: the device key path is exercised by launching the app
    // normally, and a test that reused the keychain would pass on the second
    // run for the wrong reason — because it was already paired.
    final controller = AppController(vault: Vault(MemorySecretStore()));
    await tester.pumpWidget(LevelerApp(controller: controller));
    await tester.pumpAndSettle();

    expect(find.text('配对开发机'), findsOneWidget);

    // Paste the payload the host printed, then drop focus: a focused text field
    // leaves a selection overlay above the page that swallows the next tap.
    await tester.enterText(find.byType(TextField).first, pairingPayload);
    FocusManager.instance.primaryFocus?.unfocus();
    await tester.pumpAndSettle();

    await _tapByText(tester, '读取载荷');

    // The screen must show the fingerprint the user is asked to compare, and it
    // must be the host's own — not just any sixteen hex digits.
    await _bringIntoView(tester, find.text('请在电脑上核对指纹'));
    expect(find.text('请在电脑上核对指纹'), findsOneWidget, reason: '屏幕上是：${_visibleText(tester)}');
    if (expectedHostFingerprint.isNotEmpty) {
      expect(find.text(expectedHostFingerprint), findsOneWidget,
          reason: '应当显示电脑端那把密钥的指纹，屏幕上是：${_visibleText(tester)}');
    }

    await _tapByText(tester, '指纹一致，提交配对', settle: false);

    // The property this whole design rests on: a device cannot promote its own
    // pairing. The host deliberately waits ten seconds before accepting, so for
    // the next few the app must still be unpaired and must say why.
    for (var elapsed = 0; elapsed < 4; elapsed++) {
      await tester.pump(const Duration(seconds: 1));
      await Future<void>.delayed(const Duration(seconds: 1));
      expect(controller.pairing, isNull,
          reason: '电脑还没确认，手机不该已经配对');
    }
    expect(find.textContaining('等待电脑确认'), findsOneWidget,
        reason: '等待时应当告诉用户去电脑上确认，屏幕上是：${_visibleText(tester)}');

    // Give the host up to a minute to accept, pumping so the UI keeps running.
    final deadline = DateTime.now().add(const Duration(seconds: 60));
    while (DateTime.now().isBefore(deadline) && controller.pairing == null) {
      await tester.pump(const Duration(milliseconds: 250));
      await Future<void>.delayed(const Duration(milliseconds: 250));
    }
    expect(controller.pairing, isNotNull, reason: 'the host never accepted');

    await tester.pumpAndSettle(const Duration(seconds: 2));

    // Past pairing: the projects screen is reached only by minting a token,
    // signing an RPC, and verifying the runtime's signed answer.
    expect(find.text('项目'), findsOneWidget);
    expect(controller.connection, isNot(LinkState.untrusted),
        reason: 'a project list that failed verification must not be shown');
  });
}
