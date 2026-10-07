/// Execution Presentation Contract v1 — App conformance.
///
/// Reads the SAME fixtures the reference implementation is frozen against
/// (`testdata/execution_presentation/v1/*.json`), drives the real
/// [SessionState] event projection, and compares the semantic tree with the
/// frozen expectation.
///
/// A path that needs durable history replay is DEFERRED: the app subscribes to
/// the live event stream and a snapshot, and has no `query_session_history`
/// consumer, so it cannot rebuild a finished turn's rounds from the log. The
/// gap is asserted in its own test rather than silently skipped.
library;

import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/domain/execution_presentation.dart';
import 'package:leveler_mobile/domain/session_state.dart';

const String _fixtureDir = '../../testdata/execution_presentation/v1';

const Map<String, dynamic> _defaultSnapshot = {
  'id': 's1',
  'repository': '/repo',
  'goal': 'fixture',
  'model': null,
  'mode': 'assisted',
  'branch': null,
  'status': 'idle',
  'messages': <dynamic>[],
};

Map<String, dynamic> _readFixture(String name) =>
    jsonDecode(File('$_fixtureDir/$name').readAsStringSync())
        as Map<String, dynamic>;

List<Map<String, dynamic>> _fixtures() {
  final dir = Directory(_fixtureDir);
  final names = dir
      .listSync()
      .whereType<File>()
      .map((file) => file.uri.pathSegments.last)
      .where((name) => name.endsWith('.json'))
      .toList()
    ..sort();
  return [for (final name in names) _readFixture(name)];
}

String _wireStatus(ToolStatus status) => switch (status) {
      ToolStatus.running => 'running',
      ToolStatus.ok => 'ok',
      ToolStatus.failed => 'failed',
      ToolStatus.cancelled => 'cancelled',
      ToolStatus.unknown => 'unknown',
    };

/// The App projection of one session onto the contract's semantic tree.
Map<String, dynamic> _project(SessionState state) {
  final ordered = <({int seq, Map<String, dynamic> item})>[];
  for (final item in state.executionItems) {
    final round = item.round;
    if (round != null) {
      ordered.add((
        seq: item.seq,
        item: {
          'kind': 'execution_round',
          'model_step': round.modelStep,
          'status': round.status,
          'all_ok': round.allOk,
          'batches': round.batches,
          'tools': [
            for (final tool in round.tools)
              {'id': tool.id, 'name': tool.name, 'status': _wireStatus(tool.status)},
          ],
        },
      ));
      continue;
    }
    ordered.add((seq: item.seq, item: {'kind': item.kind, 'text': item.text}));
  }
  // A runtime-authored message is a note, never user speech (§I13).
  for (var index = 0; index < state.timeline.length; index++) {
    final item = state.timeline[index];
    if (item.kind != TimelineKind.notice) continue;
    ordered.add((
      seq: index,
      item: {'kind': 'note', 'text': item.detail},
    ));
  }
  ordered.sort((a, b) => a.seq.compareTo(b.seq));
  final items = [for (final entry in ordered) entry.item];
  final terminal = state.lastTerminal;
  if (terminal != null) items.add({'kind': 'turn_end', 'status': terminal});
  return {
    'items': items,
    'user_texts': [
      for (final item in state.timeline)
        if (item.kind == TimelineKind.user) item.detail,
    ],
    'reasoning_visible': false,
  };
}

SessionState _runPath(List<dynamic> steps) {
  final state = SessionState('s1')..applySnapshot(_defaultSnapshot);
  for (final raw in steps) {
    final step = raw as Map<String, dynamic>;
    final snapshot = step['snapshot'];
    if (snapshot != null) {
      // A snapshot is the app's own entry point (`SnapshotMessage`), not an
      // event on the session stream.
      state.applySnapshot(
        (snapshot as Map<String, dynamic>)['session'] as Map<String, dynamic>,
      );
      continue;
    }
    final event = step['event'];
    if (event == null) continue; // a `history` step: deferred, see the header.
    state.applyEvent(event as Map<String, dynamic>);
  }
  return state;
}

bool _needsHistory(List<dynamic> steps) =>
    steps.any((raw) => (raw as Map<String, dynamic>).containsKey('history'));

void main() {
  group('execution presentation contract v1 (app)', () {
    for (final fixture in _fixtures()) {
      test('${fixture['id']} — ${fixture['title']}', () {
        final paths = fixture['paths'] as Map<String, dynamic>;
        var compared = 0;
        paths.forEach((name, rawSteps) {
          final steps = rawSteps as List<dynamic>;
          if (_needsHistory(steps)) return;
          final state = _runPath(steps);
          expect(_project(state), fixture['expect'],
              reason: '${fixture['id']}/$name');
          compared += 1;
        });
        expect(compared, greaterThan(0),
            reason: '${fixture['id']}: every declared path is deferred');
      });
    }

    test('the corpus declares the frozen C-cases', () {
      final ids = _fixtures().map((fixture) => fixture['id']).toSet();
      for (var index = 1; index <= 14; index++) {
        expect(ids, contains('C$index'));
      }
    });

    test('a snapshot-opened session cannot rebuild durable rounds yet', () {
      // C10's reconnect/replay paths need `query_session_history`; the app has
      // no consumer for it, so a reopened session shows the answer but not what
      // ran. Stated instead of letting the fixture suite imply coverage.
      final state = SessionState('s1')..applySnapshot(_defaultSnapshot);
      expect(state.toolFacts, isEmpty);
      expect(state.executionItems, isEmpty);
    });
  });

  group('contract pure functions', () {
    test('all-success needs every visible call to succeed (§I7)', () {
      ToolFact fact(String id, ToolStatus status) =>
          ToolFact(id: id, name: 'read_file', status: status, seq: 0);
      final rounds = groupExecutionRounds([
        fact('a', ToolStatus.ok),
        fact('b', ToolStatus.cancelled),
      ]);
      expect(rounds.single.allOk, isFalse);
      expect(rounds.single.status, 'cancelled');
    });

    test('a runtime note is never the failure reason (§I8)', () {
      const preview = 'exit: 101\n--- stderr ---\n'
          '[execution policy] write denied\n[mutation rejected] outside\n'
          'error: 2 tests failed\n';
      expect(failureReason(preview), 'error: 2 tests failed');
      expect(commandOutputBody(preview), ['error: 2 tests failed']);
    });

    test('bookkeeping after an answer neither demotes nor promotes it (§I9)', () {
      final messages = [
        ProjectedMessage(role: 'assistant', text: '改好了。', seq: 0),
      ];
      // The runtime states the classification; the name is irrelevant. This is
      // C14's invariant at the pure-function level.
      ToolFact fact(String id, String name, int seq, AnswerEffect effect) =>
          ToolFact(id: id, name: name, status: ToolStatus.ok, seq: seq, answerEffect: effect);
      final bookkeeping = [
        fact('t1', 'update_plan', 1, AnswerEffect.bookkeeping),
        fact('t2', 'update_goal', 2, AnswerEffect.bookkeeping),
      ];
      expect(committedFinalAnswer(messages, bookkeeping), '改好了。');
      // A name the app has never heard of is bookkeeping when the runtime says
      // so — the app does not classify it.
      expect(
        committedFinalAnswer(messages, [
          fact('t3', 'brand_new_tool', 1, AnswerEffect.bookkeeping),
        ]),
        '改好了。',
      );
      // A name that looks like bookkeeping demotes when the runtime says work.
      expect(
        committedFinalAnswer(messages, [
          fact('t4', 'update_plan', 1, AnswerEffect.work),
        ]),
        isNull,
      );
      final work = [
        fact('t5', 'read_file', 1, AnswerEffect.work),
      ];
      expect(committedFinalAnswer(messages, work), isNull);
      expect(
        turnTerminalFromEvent('turn_completed', messages, work),
        'no_final_answer',
      );
    });
  });
}
