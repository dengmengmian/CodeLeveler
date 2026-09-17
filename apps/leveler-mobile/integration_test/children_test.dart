/// A delegated child, watched and stopped from a phone, with a real model.
///
/// Unlike the other journeys the model is not scripted: a child's rounds go
/// through the same provider as its parent, and what this checks is the
/// runtime's record of a child reaching the phone — its state, its stop, and
/// the phone's one control over it — not any text the model chooses. So the
/// test waits on facts (a child exists, it is running, it settled as
/// cancelled) and never on wording.
///
/// The restart in the middle is the reconnect case: a fresh controller that
/// never saw a live event must rebuild the child from the session snapshot.
///
/// Run it through `scripts/simulator_pairing.sh` with `HOST_CONFIG` pointing
/// at a config that has a real model, and `JOURNEYS=children_test`.
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/main.dart';
import 'package:leveler_mobile/ui/children_sheet.dart';

import 'harness.dart';

const _task = 'Call spawn_agent exactly once, in the background (run_in_background=true), with role '
    '"explorer" and this task: "Read every file under notes/ one file per round, in name order, and '
    'report one line per file." While it runs, do not read those files yourself. When it settles, '
    'say in one sentence how it ended.';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('a child is shown, survives a reconnect, and is stopped from the phone', (tester) async {
    final store = MemorySecretStore();
    final first = await pairAndReachProjects(tester, store: store);
    final project = first.projects.firstWhere((p) => p.isOnline);
    await enterProject(tester, first, project);
    await startSession(tester, first, '验收：子 Agent');
    final sessionId = first.session!.sessionId;

    await sendMessage(tester, _task);
    await until(tester, () => first.session!.openChildren.isNotEmpty,
        limit: const Duration(seconds: 120), what: '子 Agent 启动', controller: first);
    await settleUi(tester);
    expect(find.textContaining('子 Agent · 1 未结束'), findsOneWidget,
        reason: '子 Agent 条没有出现，屏幕上是：${visibleText(tester)}');

    // One entry for the child: no separate "启动子 Agent" tool row beside it.
    final live = first.session!;
    expect(live.timeline.where((item) => item.kind == TimelineKind.subAgent), hasLength(1));
    expect(live.timeline.where((item) => item.title == '启动子 Agent'), isEmpty);

    // ---- Reconnect: a controller that never saw the live events. ----
    await tester.pumpWidget(const SizedBox.shrink());
    await settleUi(tester);
    final second = AppController(vault: Vault(store));
    await tester.pumpWidget(LevelerApp(controller: second));
    await settleUi(tester);
    await until(tester, () => second.isPaired, what: '重连后恢复配对');
    await until(tester, () => second.projects.isNotEmpty,
        limit: const Duration(seconds: 30), what: '重连后的项目列表');
    await enterProject(tester, second, second.projects.firstWhere((p) => p.id == project.id));
    final summary = second.sessions.firstWhere((s) => s.id == sessionId);
    await tapByText(tester, summary.goal, settle: false);
    await until(tester, () => second.session?.children.isNotEmpty ?? false,
        limit: const Duration(seconds: 60), what: '快照带回子 Agent', controller: second);

    final restored = second.session!.children.values.single;
    expect(restored.state, 'running', reason: '回合还在跑，快照里的子 Agent 应当是运行中：${restored.statusLabel}');
    expect(restored.role, 'explorer');
    expect(restored.readOnly, isTrue);

    // ---- Stop it from the phone. ----
    await settleUi(tester);
    await tester.tap(find.textContaining('子 Agent · 1 未结束'));
    await settleUi(tester);
    expect(find.byType(ChildrenSheet), findsOneWidget);
    final stop = find.widgetWithText(OutlinedButton, '停止');
    expect(stop, findsOneWidget, reason: '运行中的子 Agent 应当能停止，屏幕上是：${visibleText(tester)}');
    await tester.tap(stop);
    await tester.pump();

    await until(tester, () => second.session!.children[restored.id]!.state == 'settled',
        limit: const Duration(seconds: 90), what: '子 Agent 停下来', controller: second);
    final settled = second.session!.children[restored.id]!;
    expect(settled.stop, 'cancelled', reason: '手机停掉的子 Agent 应当以 cancelled 结束：${settled.statusLabel}');
    expect(find.textContaining('已取消'), findsWidgets);
    expect(second.lastError, isNull, reason: '停止子 Agent 不该报错：${second.lastError}');

    // The parent keeps running and ends its own turn.
    await until(tester, () => second.session!.status != 'running',
        limit: const Duration(seconds: 180), what: '父回合结束', controller: second);
    // Reopened mid-answer, the view may be stale until the turn ends; it must
    // then fetch a snapshot and settle, not keep showing "resynchronising".
    await until(tester, () => !second.session!.needsResync,
        limit: const Duration(seconds: 30), what: '回合结束后视图重新同步', controller: second);
    expect(second.session!.children[restored.id]!.stop, 'cancelled',
        reason: '重新同步之后子 Agent 仍应是 cancelled');
  });
}
