/// One whole conversation, driven through the real UI on a real simulator
/// against a real relay and a real agent.
///
/// Everything below the app is genuine: the driving script starts a relay,
/// enrolls a host, runs the agent, and accepts the pairing from the terminal
/// while this test waits for it. What that buys over the unit tests is the
/// parts only a device has — the keychain, the HTTP stack, the platform's
/// TLS-less loopback — and the parts only a human flow has: that the
/// fingerprint a user is asked to compare actually appears, that the app waits
/// for an accept it cannot give itself, and that a question typed on a phone
/// comes back as rendered text.
///
/// The one thing held still is the model: the host answers from
/// `scripts/scripted_provider.py`, so this can assert what should be on screen.
/// A test that cannot say that cannot tell a rendering bug from a model having
/// a different idea.
///
/// Run it through `scripts/simulator_pairing.sh` or
/// `scripts/tui_remote_pairing.py`; it needs a live host.
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';

import 'harness.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('a phone pairs, talks, and approves — in Chinese and English', (tester) async {
    final controller = await pairAndReachProjects(
      tester,
      store: persistPairing ? KeystoreSecretStore() : MemorySecretStore(),
    );

    // Entering a project is the first thing that needs a *session stream*, and
    // a stream needs an authorized WebSocket. Stopping at the project list left
    // that untested — and untested it was broken: the socket carried no token,
    // the upgrade was refused, and the screen said "loading" forever.
    final online = controller.projects.where((project) => project.isOnline).toList();
    expect(online, isNotEmpty, reason: '电脑上要有一个在线项目才能验这一步');
    await enterProject(tester, controller, online.first);

    await startSession(tester, controller, '验收：中英文与审批');

    // ---- Chinese in, Chinese out, rendered as Markdown. ----
    await sendMessage(tester, '你好，介绍一下这个项目');
    await until(tester, () => assistantSource(tester).contains('列表第二项'),
        limit: const Duration(seconds: 90), what: '中文回答');
    await tester.pumpAndSettle();

    final chineseOnScreen = renderedText(tester);
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
    await sendMessage(tester, 'now answer in English please');
    await until(tester, () => assistantSource(tester).contains('second item'),
        limit: const Duration(seconds: 90), what: 'English answer');
    await tester.pumpAndSettle();

    final englishOnScreen = renderedText(tester);
    expect(englishOnScreen, contains('English answer'));
    expect(englishOnScreen, contains('second item'));
    expect(englishOnScreen, isNot(contains('## ')));

    // ---- An approval: the point of the whole product. ----
    await sendMessage(tester, '请删除 scratch.txt');
    await until(tester, () => controller.session!.approvals.isNotEmpty,
        limit: const Duration(seconds: 90), what: '审批请求');
    await tester.pumpAndSettle();

    expect(find.text('需要你批准'), findsOneWidget,
        reason: '审批卡片没有出现，屏幕上是：${visibleText(tester)}');
    expect(find.textContaining('rm'), findsWidgets,
        reason: '审批卡片应当显示要执行的命令');
    // No "always allow" on a phone. It would write a rule into the repository
    // that outlives this pairing, and the host refuses it from a remote client
    // — so the button could only ever produce a failure nobody can explain.
    expect(find.text('始终允许'), findsNothing);
    expect(find.textContaining('始终'), findsWidgets,
        reason: '应当解释为什么没有「始终允许」，屏幕上是：${visibleText(tester)}');

    final approvalId = controller.session!.approvals.keys.first;
    await tapByText(tester, '允许一次', settle: false);

    await until(tester, () => !controller.session!.approvals.containsKey(approvalId),
        what: '审批被消化');

    // The run continues past the approval and finishes: an approval that is
    // accepted but never unblocks the turn looks identical on screen until the
    // reply that should follow it never arrives.
    await until(tester, () => renderedText(tester).contains('任务完成'),
        limit: const Duration(seconds: 120), what: '批准后的收尾回答');

    expect(controller.lastError, isNull, reason: '整段流程不该报错：${controller.lastError}');
    expect(controller.session!.needsResync, isFalse,
        reason: '正常一轮之后不该处于重新同步状态：${controller.session!.unknownEvents}');
  });
}
