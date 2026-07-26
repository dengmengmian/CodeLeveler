/// The whole thing, driven through the real UI on a real simulator against a
/// real relay and a real agent.
///
/// Everything below the app is genuine: `scripts/simulator_pairing.sh` starts a
/// relay, enrolls a host, runs `leveler remote agent`, and accepts the pairing
/// from the terminal while this test waits for it. What that buys over the unit
/// tests is the parts only a device has — the keychain, the HTTP stack, the
/// platform's TLS-less loopback — and the parts only a human flow has: that the
/// fingerprint a user is asked to compare actually appears, that the app waits
/// for an accept it cannot give itself, and that a question typed on a phone
/// comes back as rendered text.
///
/// The one thing held still is the model: the host answers from
/// `scripts/scripted_provider.py`, so this can assert what should be on screen.
/// A test that cannot say that cannot tell a rendering bug from a model having a
/// different idea.
///
/// Run it through the script, not directly; it needs a live host.
library;

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
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

/// Keep the pairing in the platform keystore instead of throwing it away.
///
/// Off by default: a test that reused the keychain would pass on its second run
/// for the wrong reason — because the device was already paired. Turned on when
/// the point is to leave a paired app behind to poke at by hand.
const bool persistPairing = bool.fromEnvironment('PERSIST_PAIRING');

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
    await tester.drag(find.byType(ListView).first, const Offset(0, -220));
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

/// Wait for something the host decides, pumping real frames meanwhile.
///
/// `pumpAndSettle` cannot be used for these: it returns as soon as the UI stops
/// animating, which is immediately, long before a reply crosses the relay.
Future<void> _until(
  WidgetTester tester,
  bool Function() done, {
  Duration limit = const Duration(seconds: 45),
  required String what,
}) async {
  final deadline = DateTime.now().add(limit);
  while (DateTime.now().isBefore(deadline)) {
    if (done()) return;
    await tester.pump(const Duration(milliseconds: 200));
    await Future<void>.delayed(const Duration(milliseconds: 200));
  }
  fail('等了 ${limit.inSeconds} 秒也没等到：$what；屏幕上是：${_visibleText(tester)}');
}

/// The Markdown the assistant bubbles were handed.
///
/// Read out of the `MarkdownBody` widgets rather than the controller, so a
/// transcript that is correct in memory but never reaches the screen still
/// fails. This is the renderer's *input*, so it still carries `##` and `-`;
/// use [_renderedText] to ask what a person would actually see.
String _assistantSource(WidgetTester tester) => tester
    .widgetList<MarkdownBody>(find.byType(MarkdownBody))
    .map((body) => body.data)
    .join('\n---\n');

/// Every glyph laid out on screen, markdown included.
///
/// `RichText` is where text finally becomes something a person can read, so a
/// heading that never got parsed shows up here as a literal `## ` — which is
/// the difference between rendered Markdown and printed Markdown.
String _renderedText(WidgetTester tester) => tester
    .widgetList<RichText>(find.byType(RichText))
    .map((rich) => rich.text.toPlainText())
    .join('\n');

/// Type into the composer and send.
///
/// `enterText` goes through the real text input channel, which is what makes
/// this meaningful for Chinese: the string crosses the platform boundary as
/// UTF-16 and comes back through the engine's codec, the path that crashed the
/// app when a malformed string arrived from the pasteboard.
Future<void> _sendMessage(WidgetTester tester, String text) async {
  final composer = find.byType(TextField).last;
  expect(composer, findsOneWidget, reason: '找不到输入框，屏幕上是：${_visibleText(tester)}');

  // Tap first. `enterText` alone only marks the field as the one to type into;
  // the characters travel over a text-input connection that exists only while
  // the field really holds focus, and after an earlier `unfocus()` there is
  // none — the text then goes nowhere and the field stays empty, which is
  // exactly how the second message in this test used to vanish.
  await tester.tap(composer);
  await tester.pumpAndSettle();
  await tester.enterText(composer, text);
  await tester.pumpAndSettle();
  expect(
    tester.widget<TextField>(composer).controller?.text,
    text,
    reason: '输入框没有收下「$text」；屏幕上是：${_visibleText(tester)}',
  );

  await tester.tap(find.byTooltip('发送'));
  await tester.pump();
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('a phone pairs, talks, and approves — in Chinese and English', (tester) async {
    expect(pairingPayload, isNotEmpty,
        reason: 'run this through scripts/simulator_pairing.sh');

    final controller = AppController(
      vault: Vault(persistPairing ? KeystoreSecretStore() : MemorySecretStore()),
    );
    if (persistPairing) {
      // Start from nothing, so this is a real pairing rather than a leftover.
      await controller.restore();
      await controller.unpair();
    }
    await tester.pumpWidget(LevelerApp(controller: controller));
    await tester.pumpAndSettle();

    expect(find.text('配对开发机'), findsOneWidget);

    // Paste the payload the host printed, then drop focus: a focused text field
    // leaves a selection overlay above the page that swallows the next tap.
    await tester.enterText(find.byType(TextField).first, pairingPayload);
    FocusManager.instance.primaryFocus?.unfocus();
    await tester.pumpAndSettle();

    await _tapByText(tester, '读取粘贴的载荷');

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
    // pairing. The host deliberately waits before accepting, so for the next few
    // seconds the app must still be unpaired and must say why.
    for (var elapsed = 0; elapsed < 4; elapsed++) {
      await tester.pump(const Duration(seconds: 1));
      await Future<void>.delayed(const Duration(seconds: 1));
      expect(controller.pairing, isNull, reason: '电脑还没确认，手机不该已经配对');
    }
    expect(find.textContaining('等待电脑确认'), findsOneWidget,
        reason: '等待时应当告诉用户去电脑上确认，屏幕上是：${_visibleText(tester)}');

    await _until(tester, () => controller.pairing != null,
        limit: const Duration(seconds: 60), what: '电脑确认配对');
    await tester.pumpAndSettle(const Duration(seconds: 2));

    // Past pairing: the projects screen is reached only by minting a token,
    // signing an RPC, and verifying the runtime's signed answer.
    expect(find.text('项目'), findsOneWidget);
    expect(controller.connection, isNot(LinkState.untrusted),
        reason: 'a project list that failed verification must not be shown');

    // Entering a project is the first thing that needs a *session stream*, and
    // a stream needs an authorized WebSocket. Stopping at the project list left
    // that untested — and untested it was broken: the socket carried no token,
    // the upgrade was refused, and the screen said "loading" forever.
    final online = controller.projects.where((project) => project.isOnline).toList();
    expect(online, isNotEmpty, reason: '电脑上要有一个在线项目才能验这一步');
    await _tapByText(tester, online.first.display, settle: false);

    await _until(tester, () => !controller.sessionsLoading, what: '会话列表');
    expect(controller.lastError, isNull, reason: '进入项目不该报错：${controller.lastError}');

    // ---- A session, started from the phone. ----
    await _tapByText(tester, '新会话');
    await tester.enterText(find.byType(TextField).last, '验收：中英文与审批');
    FocusManager.instance.primaryFocus?.unfocus();
    await tester.pumpAndSettle();
    await _tapByText(tester, '开始', settle: false);

    await _until(tester, () => controller.session != null, what: '会话建立');
    await tester.pumpAndSettle();
    expect(find.byTooltip('发送'), findsOneWidget,
        reason: '会话界面应当有输入框，屏幕上是：${_visibleText(tester)}');

    // ---- Chinese in, Chinese out, rendered as Markdown. ----
    await _sendMessage(tester, '你好，介绍一下这个项目');
    await _until(tester, () => _assistantSource(tester).contains('列表第二项'),
        limit: const Duration(seconds: 90), what: '中文回答');
    await tester.pumpAndSettle();

    final chineseOnScreen = _renderedText(tester);
    expect(chineseOnScreen, contains('中文回答'), reason: '中文回答没有出现在屏幕上');
    expect(chineseOnScreen, contains('列表第二项'));
    expect(chineseOnScreen, contains('println!("你好")'), reason: '代码块没有渲染出来');
    // Markdown must be *rendered*, not printed: the heading marker belongs to
    // the layout, not to the text a person reads.
    expect(chineseOnScreen, isNot(contains('## ')),
        reason: 'Markdown 没有渲染，标题带着 ## 直接显示了：$chineseOnScreen');
    expect(chineseOnScreen, contains('你好，介绍一下这个项目'),
        reason: '自己发的中文没有出现在对话里');

    // ---- English, same round trip. ----
    await _sendMessage(tester, 'now answer in English please');
    await _until(tester, () => _assistantSource(tester).contains('second item'),
        limit: const Duration(seconds: 90), what: 'English answer');
    await tester.pumpAndSettle();

    final englishOnScreen = _renderedText(tester);
    expect(englishOnScreen, contains('English answer'));
    expect(englishOnScreen, contains('second item'));
    expect(englishOnScreen, isNot(contains('## ')));

    // ---- An approval: the point of the whole product. ----
    await _sendMessage(tester, '请删除 scratch.txt');
    await _until(tester, () => controller.session!.approvals.isNotEmpty,
        limit: const Duration(seconds: 90), what: '审批请求');
    await tester.pumpAndSettle();

    expect(find.text('需要你批准'), findsOneWidget,
        reason: '审批卡片没有出现，屏幕上是：${_visibleText(tester)}');
    expect(find.textContaining('rm'), findsWidgets,
        reason: '审批卡片应当显示要执行的命令');

    final approvalId = controller.session!.approvals.keys.first;
    await _tapByText(tester, '允许一次', settle: false);

    await _until(tester, () => !controller.session!.approvals.containsKey(approvalId),
        what: '审批被消化');

    // The run continues past the approval and finishes: an approval that is
    // accepted but never unblocks the turn looks identical on screen until the
    // reply that should follow it never arrives.
    await _until(tester, () => _renderedText(tester).contains('任务完成'),
        limit: const Duration(seconds: 120), what: '批准后的收尾回答');

    expect(controller.lastError, isNull, reason: '整段流程不该报错：${controller.lastError}');
    expect(controller.session!.needsResync, isFalse,
        reason: '正常一轮之后不该处于重新同步状态：${controller.session!.unknownEvents}');
  });
}
