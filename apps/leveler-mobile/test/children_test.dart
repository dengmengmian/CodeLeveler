/// Children on the phone come from the runtime's facts: the snapshot's
/// `children` and the typed live events. Never from prose, never duplicated.
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/protocol/commands.dart';

Map<String, dynamic> _started(String id, {bool background = true}) => {
      'type': 'sub_agent_updated',
      'id': id,
      'nickname': 'Euclid',
      'role': 'explorer',
      'done': false,
      'ok': false,
      'detail': 'Read internal/sink',
      'profile_id': 'explorer',
      'read_only': true,
      'background': background,
    };

Map<String, dynamic> _finished(String id, {String outcome = 'completed_with_findings', String stop = 'completed'}) => {
      'type': 'sub_agent_updated',
      'id': id,
      'nickname': 'Euclid',
      'role': 'explorer',
      'done': true,
      'ok': stop == 'completed',
      'detail': 'sink exports For and Sink',
      'outcome': outcome,
      'stop': stop,
    };

Map<String, dynamic> _snapshot({List<Map<String, dynamic>> children = const [], List<Map<String, dynamic>> messages = const []}) => {
      'status': 'idle',
      'messages': messages,
      'pending_interactions': const [],
      'children': children,
    };

Map<String, dynamic> _child(String id, String state, {String? outcome, String? stop, bool ok = false}) => {
      'id': id,
      'nickname': 'Newton',
      'role': 'worker',
      'profile_id': 'worker',
      'read_only': false,
      'purpose': 'Write NOTES.md',
      'state': state,
      'ok': ok,
      'background': true,
      'scope': ['internal/sink/NOTES.md'],
      'resumes': 1,
      if (outcome != null) 'outcome': outcome,
      if (stop != null) 'stop': stop,
      'summary': state == 'settled' ? 'wrote it' : null,
      'input_tokens': 1200,
      'output_tokens': 80,
      'cost_usd_micros': 350,
    };

void main() {
  group('one entry per child', () {
    test('an accepted background spawn is the child row, not also a tool start and a tool result', () {
      final state = SessionState('s1');
      state.applyEvent(_started('c1'));
      // The host pairs the background acknowledgement with a synthesized start.
      state.applyEvent({'type': 'tool_call_started', 'id': 'call-1', 'name': 'spawn_agent', 'arguments': ''});
      state.applyEvent({'type': 'tool_call_completed', 'id': 'call-1', 'ok': true, 'preview': 'Sub-agent Euclid started'});

      expect(state.timeline.map((item) => item.kind), [TimelineKind.subAgent]);
    });

    test('a refused spawn still shows, because no child row exists for it', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'tool_call_started', 'id': 'call-2', 'name': 'spawn_agent', 'arguments': ''});
      state.applyEvent({'type': 'tool_call_completed', 'id': 'call-2', 'ok': false, 'preview': 'overlaps a worker'});

      expect(state.timeline, hasLength(1));
      expect(state.timeline.single.kind, TimelineKind.toolResult);
      expect(state.timeline.single.ok, isFalse);
      expect(state.timeline.single.detail, 'overlaps a worker');
    });
  });

  group('typed state', () {
    test('a terminal is labelled from outcome and stop, not from ok alone', () {
      final state = SessionState('s1');
      state.applyEvent(_started('c1'));
      state.applyEvent(_finished('c1', outcome: 'incomplete_partial', stop: 'budget'));

      final child = state.children['c1']!;
      expect(child.state, 'settled');
      expect(child.statusLabel, '部分结果 · 预算耗尽');
      expect(state.timeline.single.detail, startsWith('部分结果 · 预算耗尽'));
      expect(state.timeline.single.ok, isFalse);
    });

    test('a cancelled child reads cancelled, a completed one reads its findings', () {
      final state = SessionState('s1')
        ..applyEvent(_started('a'))
        ..applyEvent(_finished('a', outcome: 'incomplete_no_result', stop: 'cancelled'))
        ..applyEvent(_started('b'))
        ..applyEvent(_finished('b'));

      expect(state.children['a']!.statusLabel, '无结果 · 已取消');
      expect(state.children['b']!.statusLabel, '已完成 · 有发现');
    });

    test('a terminal recorded before outcomes were typed falls back to its ok bit', () {
      final state = SessionState('s1')
        ..applyEvent(_started('c1'))
        ..applyEvent({'type': 'sub_agent_updated', 'id': 'c1', 'nickname': 'E', 'role': 'explorer',
          'done': true, 'ok': true, 'detail': 'done'});

      expect(state.children['c1']!.statusLabel, '已完成');
    });

    test('interrupted and resumed move an open child, and a settled child never reopens', () {
      final state = SessionState('s1')..applyEvent(_started('c1'));
      state.applyEvent({'type': 'sub_agent_state_changed', 'id': 'c1', 'state': 'interrupted'});
      expect(state.children['c1']!.statusLabel, '已中断');
      expect(state.children['c1']!.canCancel, isFalse);

      state.applyEvent({'type': 'sub_agent_state_changed', 'id': 'c1', 'state': 'running'});
      expect(state.children['c1']!.canCancel, isTrue);

      state.applyEvent(_finished('c1'));
      state.applyEvent({'type': 'sub_agent_state_changed', 'id': 'c1', 'state': 'running'});
      state.applyEvent(_started('c1'));
      expect(state.children['c1']!.state, 'settled');
      expect(state.unknownEvents, isEmpty);
    });

    test('progress carries the child usage', () {
      final state = SessionState('s1')
        ..applyEvent(_started('c1'))
        ..applyEvent({'type': 'sub_agent_progress', 'id': 'c1', 'active': true,
          'input_tokens': 900, 'output_tokens': 40, 'cached_input_tokens': 700});

      final child = state.children['c1']!;
      expect((child.inputTokens, child.outputTokens), (900, 40));
    });
  });

  group('snapshot and reconnect', () {
    test('a snapshot restores children the phone never saw live', () {
      final state = SessionState('s1')
        ..applySnapshot(_snapshot(children: [
          _child('w1', 'interrupted'),
          _child('w2', 'settled', outcome: 'completed_with_findings', stop: 'completed', ok: true),
        ]));

      expect(state.children.keys, ['w1', 'w2']);
      final open = state.children['w1']!;
      expect(open.statusLabel, '已中断');
      expect(open.scope, ['internal/sink/NOTES.md']);
      expect(open.resumes, 1);
      expect(open.costUsdMicros, 350);
      expect(state.children['w2']!.statusLabel, '已完成 · 有发现');
      expect(state.openChildren.map((child) => child.id), ['w1']);
    });

    test('a stale snapshot does not reopen a child whose terminal already arrived', () {
      final state = SessionState('s1')
        ..applyEvent(_started('c1'))
        ..applyEvent(_finished('c1'));

      state.applySnapshot(_snapshot(children: [_child('c1', 'running')]));

      expect(state.children['c1']!.state, 'settled');
    });

    test('a child started after the snapshot was taken survives it', () {
      final state = SessionState('s1')..applyEvent(_started('late'));

      state.applySnapshot(_snapshot(children: const []));

      expect(state.children.keys, ['late']);
    });

    test('a runtime notice in history is a notice, not something the user typed', () {
      final state = SessionState('s1')
        ..applySnapshot(_snapshot(messages: [
          {'id': 'm1', 'role': 'user', 'text': '做个调查'},
          {
            'id': 'm2',
            'role': 'user',
            'kind': 'runtime_notice',
            'text': '## Background sub-agent settled\n\nEuclid finished: sink exports For.',
          },
        ]));

      expect(state.timeline.map((item) => item.kind), [TimelineKind.user, TimelineKind.notice]);
      expect(state.timeline.last.title, 'Background sub-agent settled');
    });
  });

  test('cancel_child matches the host schema', () {
    expect(Commands.cancelChild(sessionId: 's1', childId: 'c1'), {
      'type': 'cancel_child',
      'session_id': 's1',
      'child_id': 'c1',
    });
  });
}
