/// Walk the app slowly, so someone outside can photograph it.
///
/// The other journeys assert. This one only arrives, and waits — long enough
/// for `scripts/screenshots.sh` to catch each screen with `simctl io
/// screenshot`. Assertions can tell you the text is in the widget tree; they
/// cannot tell you a bubble is squeezed to one character per line, that a code
/// block runs off the right edge, or that the approval buttons wrapped into a
/// column of stubs. Those need eyes.
///
/// Run it through `scripts/screenshots.sh`, which starts the host, snaps every
/// second, and leaves the images in one directory.
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';

import 'harness.dart';

/// How long each screen stays up. Long enough that a one-second snapper cannot
/// miss it, short enough that the whole walk stays under the host's patience.
const Duration hold = Duration(seconds: 8);

/// Tap something if it is there, and say so if it is not.
///
/// This walk exists to produce pictures; a control that has already gone (a
/// stop button whose turn just ended) should cost one screen, not the rest of
/// them.
Future<bool> tapIfPresent(WidgetTester tester, Finder target) async {
  if (target.evaluate().isEmpty) {
    // ignore: avoid_print
    print('SKIP: 找不到控件，跳过这一步');
    return false;
  }
  await tester.tap(target);
  await tester.pump();
  return true;
}

Future<void> pause(WidgetTester tester, String label) async {
  // ignore: avoid_print — the log is how the snapper names its files.
  print('SCREEN: $label');
  final until = DateTime.now().add(hold);
  while (DateTime.now().isBefore(until)) {
    await tester.pump(const Duration(milliseconds: 200));
    await Future<void>.delayed(const Duration(milliseconds: 200));
  }
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('walk every screen slowly', (tester) async {
    final controller = await pairAndReachProjects(
      tester,
      store: persistPairing ? KeystoreSecretStore() : MemorySecretStore(),
    );
    await pause(tester, '项目列表');

    final online = controller.projects.where((project) => project.isOnline).toList();
    await enterProject(tester, controller, online.first);
    await pause(tester, '会话列表');

    await startSession(tester, controller, '看看排版：中英文、代码块、长行');
    await pause(tester, '空会话');

    // Markdown with everything a reply really contains: a heading, a list, and
    // a fenced code block whose lines are longer than the bubble.
    await sendMessage(tester, '你好，介绍一下这个项目');
    await until(tester, () => assistantSource(tester).contains('列表第二项'),
        limit: const Duration(seconds: 90), what: '中文回答', controller: controller);
    await pause(tester, '中文-markdown');

    await sendMessage(tester, 'now answer in English please');
    await until(tester, () => assistantSource(tester).contains('second item'),
        limit: const Duration(seconds: 90), what: 'English answer', controller: controller);
    await pause(tester, '英文-markdown');

    // The approval card, which is the screen a mis-tap costs the most on.
    await sendMessage(tester, '请删除 scratch.txt');
    await until(tester, () => controller.session!.approvals.isNotEmpty,
        limit: const Duration(seconds: 90), what: '审批请求', controller: controller);
    await settleUi(tester);
    await pause(tester, '审批卡片');

    await tapByText(tester, '允许一次', settle: false);
    await until(tester, () => renderedText(tester).contains('任务完成'),
        limit: const Duration(seconds: 120), what: '收尾', controller: controller);
    await pause(tester, '审批之后');

    // A turn in flight: the activity line and the stop button.
    await tester.tap(find.byIcon(Icons.arrow_back));
    await settleUi(tester);
    await until(tester, () => controller.session == null, what: '退回会话列表');
    await startSession(tester, controller, '看看运行中的样子');
    await sendMessage(tester, '慢慢讲一下这个项目');
    await until(tester, () => controller.session!.status == 'running',
        limit: const Duration(seconds: 60), what: '回合开始', controller: controller);
    await pause(tester, '运行中');

    await tapIfPresent(tester, find.byTooltip('取消当前回合'));
    await pause(tester, '取消之后');

    // Settings, where a user goes to forget this installation.
    await tapIfPresent(tester, find.byTooltip('设置').first);
    await settleUi(tester);
    await pause(tester, '设置');
  });
}
