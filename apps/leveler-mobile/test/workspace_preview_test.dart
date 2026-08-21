/// Write PNGs of Home and Timeline so a human can look at the closure UI
/// without pairing a host.
library;

import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/ui/chat_screen.dart';
import 'package:leveler_mobile/ui/home_screen.dart';
import 'package:leveler_mobile/ui/theme.dart';

final _out = Directory('/tmp/leveler-mobile-preview');

Future<void> _save(WidgetTester tester, String name) async {
  final boundary = tester.renderObject(find.byKey(_previewKey)) as RenderRepaintBoundary;
  final image = await boundary.toImage(pixelRatio: 2);
  final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
  _out.createSync(recursive: true);
  File('${_out.path}/$name.png').writeAsBytesSync(bytes!.buffer.asUint8List());
}

const _previewKey = ValueKey('preview-root');

Widget _wrap(Widget child) => MaterialApp(
      theme: levelerTheme(Brightness.light),
      home: RepaintBoundary(
        key: _previewKey,
        child: MediaQuery(
          data: const MediaQueryData(size: Size(390, 844)),
          child: child,
        ),
      ),
    );

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('preview home workspace', (tester) async {
    tester.view.physicalSize = const Size(390 * 2, 844 * 2);
    tester.view.devicePixelRatio = 2;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    final controller = AppController(vault: Vault(MemorySecretStore()));
    controller.hostName = 'studio.local';
    controller.projects = [
      ProjectSummary(id: 'p1', display: 'CodeLeveler', status: 'online'),
      ProjectSummary(id: 'p2', display: 'MuxLayer', status: 'online'),
      ProjectSummary(id: 'p3', display: 'Website', status: 'offline'),
    ];
    controller.sessionsByProject['p1'] = [
      SessionSummary(
        id: 's1',
        goal: '实现 Browser Agent 能力',
        status: 'running',
        updatedAt: DateTime.now().toUtc().toIso8601String(),
      ),
    ];
    controller.sessionsByProject['p2'] = [
      SessionSummary(
        id: 's2',
        goal: 'Fix Provider Issue',
        status: 'blocked',
        updatedAt: DateTime.now().toUtc().toIso8601String(),
      ),
    ];

    await tester.pumpWidget(_wrap(HomeScreen(controller: controller)));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    await tester.runAsync(() => _save(tester, '01-home'));
  });

  testWidgets('preview agent timeline', (tester) async {
    tester.view.physicalSize = const Size(390 * 2, 844 * 2);
    tester.view.devicePixelRatio = 2;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    final session = SessionState('s1')
      ..goal = '实现 Browser Agent 能力'
      ..status = 'running';
    session.applyEvent({
      'type': 'user_message_added',
      'message': {'id': 'u1', 'role': 'user', 'text': '实现 Browser Agent 能力'},
    });
    session.applyEvent({
      'type': 'plan_updated',
      'plan': {
        'steps': [
          {'title': '查 Runtime Trait'},
          {'title': '改协议'},
        ],
      },
    });
    session.applyEvent({
      'type': 'tool_call_started',
      'id': 't1',
      'name': 'read_file',
      'arguments': '{"path":"crates/leveler-agent/src/lib.rs"}',
    });
    session.applyEvent({
      'type': 'tool_call_completed',
      'id': 't1',
      'ok': true,
      'preview': 'pub trait Runtime { ... }',
    });
    session.applyEvent({
      'type': 'sub_agent_updated',
      'id': 'c1',
      'nickname': 'explorer',
      'role': 'explorer',
      'done': true,
      'ok': true,
      'detail': '查完 Trait',
    });
    session.applyEvent({
      'type': 'assistant_message_started',
      'message_id': 'm1',
    });
    session.applyEvent({
      'type': 'assistant_text_delta',
      'message_id': 'm1',
      'delta': '任务完成。\n\nGenerated 如下。',
    });
    session.applyEvent({
      'type': 'attachment_added',
      'attachment': {
        'id': 'a1',
        'kind': 'text_file',
        'name': 'review-report.md',
        'mime_type': 'text/markdown',
        'size_bytes': 2048,
        'sha256': 'abc',
      },
    });
    session.applyEvent({
      'type': 'attachment_added',
      'attachment': {
        'id': 'a2',
        'kind': 'unknown',
        'name': 'patch.zip',
        'mime_type': 'application/zip',
        'size_bytes': 40960,
        'sha256': 'def',
      },
    });
    session.applyEvent({'type': 'agent_activity', 'label': 'Running tests'});

    final controller = AppController(vault: Vault(MemorySecretStore()))..session = session;
    await tester.pumpWidget(_wrap(ChatScreen(controller: controller)));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    await tester.runAsync(() => _save(tester, '02-timeline'));
  });
}
