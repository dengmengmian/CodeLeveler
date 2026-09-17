/// Replay one real session and check what the screen would show.
///
/// The file comes from `testdata/session_transcript.golden.json`, written by the
/// Rust types themselves. That is the point: this app's transcript logic was
/// written against field names I guessed at — `content` where the type says
/// `text`, a flattened approval where the type nests it under `request` — and
/// both guesses passed hand-written tests while rendering empty bubbles on a
/// device. A test built from my own assumptions cannot catch my assumptions.
library;

import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/domain/session_state.dart';

File goldenFile() {
  for (final path in [
    '../../testdata/session_transcript.golden.json',
    'testdata/session_transcript.golden.json',
  ]) {
    final file = File(path);
    if (file.existsSync()) return file;
  }
  throw StateError('session_transcript.golden.json not found; run from apps/leveler-mobile');
}

void main() {
  late Map<String, dynamic> golden;
  late Map<String, dynamic> expected;

  setUpAll(() {
    golden = jsonDecode(goldenFile().readAsStringSync()) as Map<String, dynamic>;
    expected = golden['expected'] as Map<String, dynamic>;
  });

  List<Map<String, String>> shown(SessionState state) => state.transcript
      .map((entry) => {'role': entry.role, 'text': entry.text})
      .toList(growable: false);

  List<Map<String, String>> wanted(String key) =>
      (expected[key] as List<dynamic>)
          .map((raw) => (raw as Map<String, dynamic>).map((k, v) => MapEntry(k, '$v')))
          .toList(growable: false);

  test('a snapshot renders the messages and the pending approval it carries', () {
    final state = SessionState('s_golden');
    state.applySnapshot(golden['snapshot'] as Map<String, dynamic>);

    expect(shown(state), wanted('transcript_after_snapshot'));
    expect(
      state.approvals.keys.toList(),
      (expected['pending_approvals_after_snapshot'] as List<dynamic>).map((id) => '$id').toList(),
      reason: 'a snapshot carries live approvals; missing them means a prompt the user never sees',
    );
  });

  test('one real turn produces exactly the bubbles it should', () {
    final state = SessionState('s_golden');
    state.applySnapshot(golden['snapshot'] as Map<String, dynamic>);
    for (final event in golden['turn'] as List<dynamic>) {
      state.applyEvent(event as Map<String, dynamic>);
    }

    expect(shown(state), wanted('transcript_after_turn'));
  });

  test('an ordinary turn never leaves the view marked stale', () {
    final state = SessionState('s_golden');
    state.applySnapshot(golden['snapshot'] as Map<String, dynamic>);
    for (final event in golden['turn'] as List<dynamic>) {
      state.applyEvent(event as Map<String, dynamic>);
    }

    // A turn carries kinds a chat UI has no use for — reasoning, token counts,
    // notifications. Treating each as a reason to resynchronise put the phone
    // in a permanent "reconnecting" state and asked the host for a snapshot
    // after every one of them.
    expect(
      state.needsResync,
      expected['needs_resync_after_turn'],
      reason: '未识别的事件：${state.unknownEvents}',
    );
  });

  test('the model reasoning never reaches the transcript', () {
    final state = SessionState('s_golden');
    for (final event in golden['turn'] as List<dynamic>) {
      state.applyEvent(event as Map<String, dynamic>);
    }
    // It is the model thinking aloud, not something it said to the user.
    for (final entry in state.transcript) {
      expect(entry.text, isNot(contains('推理')));
    }
  });
}
