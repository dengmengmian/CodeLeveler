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

  testWidgets('a session with nothing in it shows what it is for', (tester) async {
    // Not a blank screen. A user who has just typed a goal and landed here
    // needs to see that it arrived; the old empty state showed nothing at all,
    // which reads as a failure rather than as a fresh session.
    final session = SessionState('s1')
      ..applySnapshot({
        'status': 'idle',
        'goal': '把登录页的报错文案改掉',
        'messages': const [],
        'pending_interactions': const [],
      });

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    expect(find.text('把登录页的报错文案改掉'), findsOneWidget);
    expect(find.textContaining('开始这次会话'), findsOneWidget);
  });

  testWidgets('a short conversation sits against the composer', (tester) async {
    // Top-aligned, a single reply floated at the top of the screen with a
    // screenful of nothing between it and the composer. A chat reads from the
    // bottom.
    final session = SessionState('s1')
      ..applyEvent({
        'type': 'user_message_added',
        'message': {'id': 'u1', 'role': 'user', 'text': '就一句话'},
      });

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    final bubble = tester.getRect(find.text('就一句话'));
    final composer = tester.getRect(find.byType(TextField));
    final screen = tester.getSize(find.byType(ChatScreen));
    // Within a couple of bubble-heights of the composer, rather than up at the
    // top of the screen.
    expect(
      composer.top - bubble.bottom,
      lessThan(screen.height * 0.25),
      reason: '短对话不该悬在屏幕顶端，离输入框还有大半屏',
    );
  });

  testWidgets('a host notification is shown, and not as the assistant speaking',
      (tester) async {
    // Dropped silently before. The host has no other way to say something that
    // is not an answer.
    final session = SessionState('s1')
      ..applyEvent({
        'type': 'assistant_message_started',
        'message_id': 'm1',
      })
      ..applyEvent({
        'type': 'assistant_text_delta',
        'message_id': 'm1',
        'delta': '好的。',
      })
      ..applyEvent({'type': 'notification', 'level': 'warning', 'message': '磁盘快满了'});

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    expect(find.text('磁盘快满了'), findsOneWidget);
    // One Markdown body: the answer. The notice is not a second one.
    expect(find.byType(MarkdownBody), findsOneWidget);
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
    // The list is reversed, so it opens at the newest message and offset zero
    // *is* the bottom. Scrolling back through history therefore means dragging
    // downward; the old upward drag is a no-op against the end stop.
    expect(before, 0, reason: '打开时应当停在最新一条');
    // A drag that starts on a bubble must move the list. Every bubble owning a
    // SelectableText meant this drag selected text instead.
    await tester.drag(list, const Offset(0, 400));
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
