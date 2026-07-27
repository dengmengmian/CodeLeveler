/// Two projects on one host, and what survives the app being started again.
///
/// These are the two Phase 1 acceptance lines that the single-project journey
/// cannot reach. Both were proven in Rust against a fake runtime, which shows
/// the *agent* keeps projects apart — it says nothing about whether the phone
/// does. The phone holds one socket, one sequence counter and one session
/// object at a time, and rebinds all three on every switch; that is where
/// crossing wires would actually happen.
///
/// The restart is a real restart of everything the app owns: a second
/// `AppController` over the same storage, a fresh widget tree, a new socket,
/// nothing carried over in memory. The process itself is not killed — an
/// integration test dies with it — so what this does not prove is that iOS
/// hands the keychain back after a cold boot.
///
/// Run it through `scripts/simulator_pairing.sh`; it needs a host with two
/// projects open.
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/main.dart';

import 'harness.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('switching projects keeps them apart, and a restart resyncs', (tester) async {
    // Shared between the two controllers: this is the storage that outlives an
    // app launch, so the "restart" reads back exactly what the first run wrote.
    final store = persistPairing ? KeystoreSecretStore() : MemorySecretStore();
    final first = await pairAndReachProjects(tester, store: store);

    final online = first.projects.where((project) => project.isOnline).toList();
    expect(online.length, greaterThanOrEqualTo(2),
        reason: '这条用例要电脑上开着两个项目，现在只有：'
            '${first.projects.map((p) => "${p.display}(${p.status})").join(", ")}');
    final alpha = online[0];
    final beta = online[1];

    // ---- A session in the first project. ----
    await enterProject(tester, first, alpha);
    // No assertion that this project is empty: the journey before this one may
    // have left a session here, and "empty" would then be a claim about the
    // order the scripts run in rather than about the app.
    await startSession(tester, first, '验收 A');
    await sendMessage(tester, '你好，我在 A 项目');
    await until(tester, () => assistantSource(tester).contains('列表第二项'),
        limit: const Duration(seconds: 90), what: 'A 项目的回答');
    final alphaSessionId = first.session!.sessionId;

    // ---- Switch, the way a user does: back out of the chat, then out of the
    // project. Two screens, two taps on the same arrow. ----
    await tester.tap(find.byIcon(Icons.arrow_back));
    await tester.pumpAndSettle();
    await until(tester, () => first.session == null, what: '退回会话列表');
    await tester.tap(find.byIcon(Icons.arrow_back));
    await tester.pumpAndSettle();
    await until(tester, () => first.currentProjectId == null, what: '退回项目列表');

    await enterProject(tester, first, beta);

    // The point of the whole switch: the second project must not inherit the
    // first one's sessions. A phone that kept them would show a user another
    // repository's work under this repository's name.
    expect(
      first.sessions.where((session) => session.id == alphaSessionId),
      isEmpty,
      reason: 'A 的会话出现在了 B 的列表里：${first.sessions.map((s) => s.goal).join(", ")}',
    );
    expect(first.session, isNull, reason: '切换项目后不该还挂着上一个项目的会话');

    await startSession(tester, first, '验收 B');
    await sendMessage(tester, 'hello from project B');
    await until(tester, () => assistantSource(tester).contains('second item'),
        limit: const Duration(seconds: 90), what: 'B 项目的回答');
    final betaSessionId = first.session!.sessionId;

    expect(betaSessionId, isNot(alphaSessionId));
    // Events from A must not be arriving on B's stream. Asserting on the
    // rendered screen rather than on internal state: a leak that only reaches
    // the transcript is still a leak the user sees.
    final onBeta = renderedText(tester);
    expect(onBeta, contains('hello from project B'));
    expect(onBeta, isNot(contains('我在 A 项目')),
        reason: 'B 的对话里出现了 A 的消息');

    // ---- Start the app again. ----
    //
    // Everything in memory goes: a new controller, a new widget tree, a new
    // socket, a new sequence counter. Only the keystore carries over, which is
    // all a relaunched app really has.
    // Tear the tree down first. Pumping a new `LevelerApp` straight over the
    // old one keeps the existing `State` — same widget type, no key — and that
    // State already ran `restore()` against the *previous* controller, so the
    // new one would never load the pairing it is supposed to find. An empty
    // frame in between makes this a relaunch rather than a rebuild.
    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pumpAndSettle();

    final second = AppController(vault: Vault(store));
    await tester.pumpWidget(LevelerApp(controller: second));
    await tester.pumpAndSettle();

    await until(tester, () => second.isPaired, what: '重启后恢复配对');
    expect(find.text('项目'), findsOneWidget,
        reason: '重启后应当直接回到项目列表，屏幕上是：${visibleText(tester)}');

    await until(tester, () => second.projects.isNotEmpty,
        limit: const Duration(seconds: 30), what: '重启后的项目列表');
    final again = second.projects.firstWhere((project) => project.id == beta.id);
    await enterProject(tester, second, again);

    // The session that was open when the app "died" is still there, and opening
    // it brings back what was said — which can only come from a snapshot, since
    // this process never saw those events.
    final restored = second.sessions.firstWhere(
      (session) => session.id == betaSessionId,
      orElse: () => throw StateError(
          '重启后没找到刚才那个会话：${second.sessions.map((s) => s.id).join(", ")}'),
    );
    await tapByText(tester, restored.goal, settle: false);
    await until(tester, () => second.session != null, what: '重新打开会话');
    await until(tester, () => renderedText(tester).contains('hello from project B'),
        limit: const Duration(seconds: 60), what: '快照带回之前的对话');

    final afterRestart = renderedText(tester);
    expect(afterRestart, contains('second item'), reason: '助手那半边没有随快照回来');
    expect(afterRestart, isNot(contains('我在 A 项目')),
        reason: '快照把另一个项目的内容也带回来了');
    expect(second.session!.needsResync, isFalse,
        reason: '拿到快照之后不该还在等重新同步：${second.session!.unknownEvents}');
    expect(second.lastError, isNull, reason: '重启这一段不该报错：${second.lastError}');
  });
}
