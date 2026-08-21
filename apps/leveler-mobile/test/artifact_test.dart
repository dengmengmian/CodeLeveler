/// Artifacts are AttachmentRef projections. There is no public download URL.
library;

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/crypto/store.dart';
import 'package:leveler_mobile/domain/app_controller.dart';
import 'package:leveler_mobile/domain/artifact.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/ui/artifact_preview.dart';
import 'package:leveler_mobile/ui/chat_screen.dart';

AppController _controllerWith(SessionState session) {
  final controller = AppController(vault: Vault(MemorySecretStore()));
  controller.session = session;
  return controller;
}

Widget _app(AppController controller) =>
    MaterialApp(home: ChatScreen(controller: controller));

Map<String, dynamic> _attachment({
  String id = 'a1',
  String name = 'review-report.md',
  String mime = 'text/markdown',
  int size = 2048,
  String sha = 'abc123',
}) =>
    {
      'id': id,
      'kind': 'text_file',
      'name': name,
      'mime_type': mime,
      'size_bytes': size,
      'sha256': sha,
    };

void main() {
  test('mime and filename decide the product type, not a second registry', () {
    expect(
      classifyArtifact(name: 'review-report.md', mimeType: 'text/plain'),
      ArtifactType.markdown,
    );
    expect(
      classifyArtifact(name: 'change.patch', mimeType: 'text/x-diff'),
      ArtifactType.diff,
    );
    expect(
      classifyArtifact(name: 'shot.png', mimeType: 'image/png'),
      ArtifactType.image,
    );
    expect(
      classifyArtifact(name: 'out.zip', mimeType: 'application/zip'),
      ArtifactType.archive,
    );
  });

  test('attachment_added becomes an Artifact on the session, not a filename string', () {
    final state = SessionState('s1');
    state.applyEvent({
      'type': 'attachment_added',
      'attachment': _attachment(size: 186368),
    });

    expect(state.unknownEvents, isEmpty);
    expect(state.artifacts, hasLength(1));
    expect(state.artifacts.single.name, 'review-report.md');
    expect(state.artifacts.single.type, ArtifactType.markdown);
    expect(state.artifacts.single.sizeBytes, 186368);
    expect(state.artifacts.single.sha256, 'abc123');
    expect(state.timeline.single.kind, TimelineKind.attachment);
  });

  testWidgets('an artifact card shows name, type and size, and opens preview', (tester) async {
    final session = SessionState('s1')
      ..applyEvent({
        'type': 'attachment_added',
        'attachment': _attachment(),
      });

    await tester.pumpWidget(_app(_controllerWith(session)));
    await tester.pumpAndSettle();

    expect(find.text('review-report.md'), findsOneWidget);
    expect(find.textContaining('Markdown'), findsWidgets);
    expect(find.textContaining('2 KB'), findsOneWidget);

    await tester.tap(find.text('review-report.md'));
    await tester.pumpAndSettle();

    expect(find.byType(ArtifactPreviewPage), findsOneWidget);
    expect(find.textContaining('产物加载失败'), findsOneWidget);
    expect(find.textContaining('还没有连接到开发机'), findsOneWidget);
  });

  testWidgets('preview shows empty when there is no fetch and no bytes', (tester) async {
    await tester.pumpWidget(MaterialApp(
      home: ArtifactPreviewPage(artifact: Artifact.fromAttachment('s1', _attachment())),
    ));
    await tester.pumpAndSettle();
    expect(find.textContaining('没有可预览的内容'), findsOneWidget);
  });

  testWidgets('preview shows an error when fetch fails', (tester) async {
    await tester.pumpWidget(MaterialApp(
      home: ArtifactPreviewPage(
        artifact: Artifact.fromAttachment('s1', _attachment()),
        onFetch: () async => throw StateError('host refused'),
      ),
    ));
    await tester.pumpAndSettle();
    expect(find.textContaining('产物加载失败'), findsOneWidget);
    expect(find.textContaining('host refused'), findsOneWidget);
  });

  testWidgets('preview renders markdown bytes when they are already in hand', (tester) async {
    final artifact = Artifact.fromAttachment(
      's1',
      _attachment(),
    );
    await tester.pumpWidget(MaterialApp(
      home: ArtifactPreviewPage(
        artifact: artifact,
        bytes: utf8.encode('# 结论\n\n**通过**。'),
      ),
    ));
    await tester.pumpAndSettle();

    expect(find.byType(MarkdownBody), findsOneWidget);
    expect(find.textContaining('# 结论'), findsNothing);
    expect(find.textContaining('还拿不到文件内容'), findsNothing);
  });
}
