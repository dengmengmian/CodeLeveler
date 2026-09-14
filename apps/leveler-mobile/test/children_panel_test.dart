/// The children surface: what it counts, what it offers, and to whom.
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/ui/chat_screen.dart';
import 'package:leveler_mobile/ui/children_sheet.dart';

SessionState _sessionWithChildren() => SessionState('s1')
  ..applySnapshot({
    'status': 'running',
    'messages': const [],
    'pending_interactions': const [],
    'children': [
      {
        'id': 'c1', 'nickname': 'Euclid', 'role': 'explorer', 'profile_id': 'explorer', 'read_only': true,
        'purpose': 'Read internal/sink', 'state': 'running', 'background': true,
        'input_tokens': 1200, 'output_tokens': 80, 'cost_usd_micros': 350,
      },
      {
        'id': 'c2', 'nickname': 'Newton', 'role': 'worker', 'read_only': false, 'purpose': 'Write NOTES.md',
        'state': 'interrupted', 'background': true, 'scope': ['internal/sink/NOTES.md'], 'resumes': 1,
      },
      {
        'id': 'c3', 'nickname': 'Gauss', 'role': 'explorer', 'read_only': true, 'purpose': 'Survey',
        'state': 'settled', 'ok': false, 'outcome': 'incomplete_partial', 'stop': 'budget',
        'summary': 'covered 3 of 12 files',
      },
    ],
  });

Widget _sheet(SessionState session, {bool canControl = true, void Function(String)? onCancel}) => MaterialApp(
      home: Scaffold(
        body: ChildrenSheet(session: session, canControl: canControl, onCancel: onCancel ?? (_) {}),
      ),
    );

void main() {
  testWidgets('the chat screen counts children from the runtime and opens the list', (tester) async {
    final controller = AppController(vault: Vault(MemorySecretStore()))..session = _sessionWithChildren();
    await tester.pumpWidget(MaterialApp(home: ChatScreen(controller: controller)));
    await tester.pumpAndSettle();

    expect(find.text('子 Agent · 2 未结束 · 共 3'), findsOneWidget);
    await tester.tap(find.text('子 Agent · 2 未结束 · 共 3'));
    await tester.pumpAndSettle();
    expect(find.byType(ChildrenSheet), findsOneWidget);
  });

  testWidgets('no children, no strip', (tester) async {
    final controller = AppController(vault: Vault(MemorySecretStore()))
      ..session = (SessionState('s1')
        ..applySnapshot({'status': 'idle', 'messages': const [], 'pending_interactions': const []}));
    await tester.pumpWidget(MaterialApp(home: ChatScreen(controller: controller)));
    await tester.pumpAndSettle();

    expect(find.textContaining('子 Agent ·'), findsNothing);
  });

  testWidgets('stop is offered only on a running child and names that child', (tester) async {
    final cancelled = <String>[];
    await tester.pumpWidget(_sheet(_sessionWithChildren(), onCancel: cancelled.add));
    await tester.pumpAndSettle();

    expect(find.widgetWithText(OutlinedButton, '停止'), findsOneWidget);
    await tester.tap(find.widgetWithText(OutlinedButton, '停止'));
    expect(cancelled, ['c1']);
  });

  testWidgets('each child shows its recorded facts', (tester) async {
    await tester.pumpWidget(_sheet(_sessionWithChildren()));
    await tester.pumpAndSettle();

    expect(find.text('运行中'), findsOneWidget);
    expect(find.text('已中断'), findsOneWidget);
    expect(find.text('部分结果 · 预算耗尽'), findsOneWidget);
    expect(find.textContaining('只读 · 后台'), findsWidgets);
    expect(find.textContaining('范围 internal/sink/NOTES.md'), findsOneWidget);
    expect(find.textContaining('续跑 1 次'), findsOneWidget);
    expect(find.textContaining('↑1200 ↓80 · \$0.000350'), findsOneWidget);
    expect(find.text('covered 3 of 12 files'), findsOneWidget);
  });

  testWidgets('a read-only pairing sees children but gets no stop button', (tester) async {
    await tester.pumpWidget(_sheet(_sessionWithChildren(), canControl: false));
    await tester.pumpAndSettle();

    expect(find.text('运行中'), findsOneWidget);
    expect(find.widgetWithText(OutlinedButton, '停止'), findsNothing);
  });
}
