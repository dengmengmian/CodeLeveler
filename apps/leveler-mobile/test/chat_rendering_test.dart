/// What the conversation screen must do with the text it is given.
///
/// Both cases here were reported by a person looking at a screen, which is the
/// expensive way to find them: the assistant answers in Markdown and the app
/// showed the source, and a list whose every bubble owned the drag gesture
/// could not be scrolled at all.
library;

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/ui/chat_screen.dart';
import 'package:leveler_mobile/crypto/store.dart';

/// A controller with one session, enough to render the screen.
AppController _controllerWith(SessionState session) {
  final controller = AppController(vault: Vault(MemorySecretStore()));
  controller.session = session;
  return controller;
}

Widget _app(AppController controller) =>
    MaterialApp(home: ChatScreen(controller: controller));

void main() {
  testWidgets('an assistant answer is rendered as Markdown, not as its source',
      (tester) async {
    final session = SessionState('s1')
      ..applyEvent({
        'type': 'assistant_message_started',
        'message_id': 'm1',
      })
      ..applyEvent({
        'type': 'assistant_text_delta',
        'message_id': 'm1',
        'delta': '## 标题\n\n这是 **加粗** 的一段。',
      });

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    expect(find.byType(MarkdownBody), findsOneWidget);
    // The literal syntax must not survive to the screen.
    expect(find.textContaining('## 标题'), findsNothing);
    expect(find.textContaining('**加粗**'), findsNothing);
  });

  testWidgets('what the user typed stays literal', (tester) async {
    final session = SessionState('s1')
      ..applyEvent({
        'type': 'user_message_added',
        'message': {'id': 'u1', 'role': 'user', 'text': '为什么 **这里** 不加粗'},
      });

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    // A person typing asterisks meant asterisks.
    expect(find.text('为什么 **这里** 不加粗'), findsOneWidget);
    expect(find.byType(MarkdownBody), findsNothing);
  });

  testWidgets('a long conversation scrolls', (tester) async {
    final session = SessionState('s1');
    for (var i = 0; i < 40; i++) {
      session.applyEvent({
        'type': 'user_message_added',
        'message': {'id': 'u$i', 'role': 'user', 'text': '第 $i 条消息'},
      });
    }

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    final list = find.byType(ListView);
    final before = tester.widget<ListView>(list).controller!.position.pixels;
    // A drag that starts on a bubble must move the list. Every bubble owning a
    // SelectableText meant this drag selected text instead.
    await tester.drag(list, const Offset(0, -400));
    await tester.pumpAndSettle();
    final after = tester.widget<ListView>(list).controller!.position.pixels;

    expect(after, isNot(before), reason: '列表没有滚动');
    // Honest limit: a widget-test drag is a *touch* drag, and touch scrolls even
    // when each bubble owns a SelectableText — this assertion passes either way,
    // which I checked. It guards against a genuinely unscrollable list, not
    // against gesture ownership, which needs a pointer device this harness has
    // no way to simulate.
  });
}
