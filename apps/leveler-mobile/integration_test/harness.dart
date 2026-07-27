/// Shared driving for the on-device journeys.
///
/// Both integration tests start the same way — a payload from the host, a
/// fingerprint to compare, an accept typed on a terminal — and both have to
/// wait for things a phone does not decide. Keeping that in one place means the
/// two tests differ only where the journeys differ.
library;

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/main.dart';

/// Supplied by the script that runs the host side.
const String pairingPayload = String.fromEnvironment('PAIRING_PAYLOAD');

/// What the host prints for its own key, so a test can check the app shows the
/// same machine rather than merely showing *a* fingerprint.
const String expectedHostFingerprint = String.fromEnvironment('HOST_FINGERPRINT');

/// Keep the pairing in the platform keystore instead of throwing it away.
///
/// Off by default: a test that reused the keychain would pass on its second run
/// for the wrong reason — because the device was already paired. Turned on when
/// the point is to leave a paired app behind to poke at by hand.
const bool persistPairing = bool.fromEnvironment('PERSIST_PAIRING');

/// Everything the screen is currently showing, so a failed expectation says
/// what the app did instead of only what it did not do.
String visibleText(WidgetTester tester) => tester
    .widgetList<Text>(find.byType(Text))
    .map((text) => text.data ?? '')
    .where((text) => text.isNotEmpty)
    .join(' | ');

/// Every glyph laid out on screen, Markdown included.
///
/// `RichText` is where text finally becomes something a person can read, so a
/// heading that never got parsed shows up here as a literal `## ` — which is
/// the difference between rendered Markdown and printed Markdown.
String renderedText(WidgetTester tester) => tester
    .widgetList<RichText>(find.byType(RichText))
    .map((rich) => rich.text.toPlainText())
    .join('\n');

/// The Markdown the assistant bubbles were handed — the renderer's *input*, so
/// it still carries `##` and `-`. Use [renderedText] to ask what a person sees.
String assistantSource(WidgetTester tester) => tester
    .widgetList<MarkdownBody>(find.byType(MarkdownBody))
    .map((body) => body.data)
    .join('\n---\n');

/// Scroll a list item into view before touching it.
///
/// A `ListView` only builds what its viewport needs, so anything below the fold
/// is absent from the tree rather than merely invisible — and `find` reports it
/// as missing, which reads like a bug in the app instead of one in the test.
///
/// Dragging by hand rather than `scrollUntilVisible`, which throws its own
/// obscure error when the target has not been built yet — the exact case this
/// helper exists to handle.
Future<void> bringIntoView(WidgetTester tester, Finder target) async {
  for (var attempt = 0; attempt < 12; attempt++) {
    if (target.evaluate().isNotEmpty) {
      await tester.ensureVisible(target);
      await settleUi(tester);
      return;
    }
    await tester.drag(find.byType(ListView).first, const Offset(0, -220));
    await settleUi(tester);
  }
}

/// Scroll a button into view and press it, failing loudly if the press lands on
/// something else — a tap that silently misses is the difference between a test
/// that checks the app and one that checks nothing.
Future<void> tapByText(WidgetTester tester, String label, {bool settle = true}) async {
  final target = find.text(label);
  await bringIntoView(tester, target);
  expect(target, findsOneWidget, reason: '找不到「$label」，屏幕上是：${visibleText(tester)}');
  await tester.tap(target, warnIfMissed: true);
  // `settle: false` for taps that start something the *host* finishes:
  // pumpAndSettle runs real frames until the UI stops moving, which here means
  // waiting out the very window the test is trying to observe.
  if (settle) {
    await settleUi(tester);
  } else {
    await tester.pump();
  }
}

/// Let the UI catch up, without requiring it to ever stand still.
///
/// `pumpAndSettle` waits for *no* animation to be running, and a turn in flight
/// shows a spinner that never stops — so it pumps until its own timeout and
/// takes the test with it. What these tests actually want is "a few frames have
/// passed", which is what this does.
Future<void> settleUi(WidgetTester tester) async {
  try {
    await tester.pumpAndSettle(
      const Duration(milliseconds: 100),
      EnginePhase.sendSemanticsUpdate,
      const Duration(seconds: 2),
    );
  } on FlutterError {
    // Something is still animating. That is the UI being honest about work in
    // progress, not a failure.
    for (var frame = 0; frame < 5; frame++) {
      await tester.pump(const Duration(milliseconds: 100));
    }
  }
}

/// Wait for something the host decides, pumping real frames meanwhile.
///
/// `pumpAndSettle` cannot be used for these: it returns as soon as the UI stops
/// animating, which is immediately, long before a reply crosses the relay.
Future<void> until(
  WidgetTester tester,
  bool Function() done, {
  Duration limit = const Duration(seconds: 45),
  required String what,
  AppController? controller,
}) async {
  final deadline = DateTime.now().add(limit);
  while (DateTime.now().isBefore(deadline)) {
    if (done()) return;
    await tester.pump(const Duration(milliseconds: 200));
    await Future<void>.delayed(const Duration(milliseconds: 200));
  }
  // A timeout is nearly always a message that went nowhere, so say what the
  // controller thinks happened rather than only what the screen shows.
  final state = controller == null
      ? ''
      : '；连接=${controller.connection}，错误=${controller.lastError}'
          '，会话状态=${controller.session?.status}';
  fail('等了 ${limit.inSeconds} 秒也没等到：$what$state；屏幕上是：${visibleText(tester)}');
}

/// Type into the composer and send.
///
/// `enterText` goes through the real text input channel, which is what makes
/// this meaningful for Chinese: the string crosses the platform boundary as
/// UTF-16 and comes back through the engine's codec, the path that crashed the
/// app when a malformed string arrived from the pasteboard.
Future<void> sendMessage(WidgetTester tester, String text) async {
  final composer = find.byType(TextField).last;
  expect(composer, findsOneWidget, reason: '找不到输入框，屏幕上是：${visibleText(tester)}');

  // Tap first. `enterText` alone only marks the field as the one to type into;
  // the characters travel over a text-input connection that exists only while
  // the field really holds focus, and after an earlier `unfocus()` there is
  // none — the text then goes nowhere and the field stays empty.
  await tester.tap(composer);
  await settleUi(tester);
  await tester.enterText(composer, text);
  await settleUi(tester);
  expect(
    tester.widget<TextField>(composer).controller?.text,
    text,
    reason: '输入框没有收下「$text」；屏幕上是：${visibleText(tester)}',
  );

  await tester.tap(find.byTooltip('发送'));
  await tester.pump();
}

/// Start a session from the phone and wait for its stream.
Future<void> startSession(WidgetTester tester, AppController controller, String goal) async {
  await tapByText(tester, '新会话');
  await tester.enterText(find.byType(TextField).last, goal);
  FocusManager.instance.primaryFocus?.unfocus();
  await settleUi(tester);
  await tapByText(tester, '开始', settle: false);

  await until(tester, () => controller.session != null, what: '会话建立');
  await settleUi(tester);
  expect(find.byTooltip('发送'), findsOneWidget,
      reason: '会话界面应当有输入框，屏幕上是：${visibleText(tester)}');
}

/// Pair with the host and stop on the project list.
///
/// The store is returned so a test can build a *second* controller over the
/// same storage — which is what "the app was killed and started again" looks
/// like from everywhere except the operating system.
Future<AppController> pairAndReachProjects(
  WidgetTester tester, {
  required SecretStore store,
}) async {
  expect(pairingPayload, isNotEmpty, reason: '要通过 scripts/ 下的脚本来跑');

  final controller = AppController(vault: Vault(store));
  if (persistPairing) {
    // Start from nothing, so this is a real pairing rather than a leftover.
    await controller.restore();
    await controller.unpair();
  }
  await tester.pumpWidget(LevelerApp(controller: controller));
  await settleUi(tester);

  expect(find.text('配对开发机'), findsOneWidget);

  // Paste the payload the host printed, then drop focus: a focused text field
  // leaves a selection overlay above the page that swallows the next tap.
  await tester.enterText(find.byType(TextField).first, pairingPayload);
  FocusManager.instance.primaryFocus?.unfocus();
  await settleUi(tester);

  await tapByText(tester, '读取粘贴的载荷');

  // The screen must show the fingerprint the user is asked to compare, and it
  // must be the host's own — not just any sixteen hex digits.
  await bringIntoView(tester, find.text('请在电脑上核对指纹'));
  expect(find.text('请在电脑上核对指纹'), findsOneWidget, reason: '屏幕上是：${visibleText(tester)}');
  if (expectedHostFingerprint.isNotEmpty) {
    expect(find.text(expectedHostFingerprint), findsOneWidget,
        reason: '应当显示电脑端那把密钥的指纹，屏幕上是：${visibleText(tester)}');
  }

  await tapByText(tester, '指纹一致，提交配对', settle: false);

  // The property this whole design rests on: a device cannot promote its own
  // pairing. The host deliberately waits before accepting, so for the next few
  // seconds the app must still be unpaired and must say why.
  for (var elapsed = 0; elapsed < 4; elapsed++) {
    await tester.pump(const Duration(seconds: 1));
    await Future<void>.delayed(const Duration(seconds: 1));
    expect(controller.pairing, isNull, reason: '电脑还没确认，手机不该已经配对');
  }
  expect(find.textContaining('等待电脑确认'), findsOneWidget,
      reason: '等待时应当告诉用户去电脑上确认，屏幕上是：${visibleText(tester)}');

  await until(tester, () => controller.pairing != null,
      limit: const Duration(seconds: 60), what: '电脑确认配对');
  await tester.pumpAndSettle(const Duration(seconds: 2));

  // Past pairing: the projects screen is reached only by minting a token,
  // signing an RPC, and verifying the runtime's signed answer.
  expect(find.text('项目'), findsOneWidget);
  expect(controller.connection, isNot(LinkState.untrusted),
      reason: '验签失败的项目列表不该被显示');

  return controller;
}

/// Enter a project from the list and wait for its session list.
Future<void> enterProject(
  WidgetTester tester,
  AppController controller,
  ProjectSummary project,
) async {
  await tapByText(tester, project.display, settle: false);
  await until(tester, () => !controller.sessionsLoading, what: '${project.display} 的会话列表');
  expect(controller.lastError, isNull, reason: '进入项目不该报错：${controller.lastError}');
}
