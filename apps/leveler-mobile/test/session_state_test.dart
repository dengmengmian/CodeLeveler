/// What the transcript must and must not do with a stream of events.
library;

import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/domain/session_state.dart';
import 'package:leveler_mobile/protocol/ids.dart';
import 'package:leveler_mobile/protocol/pairing.dart';
import 'package:leveler_mobile/protocol/wire.dart';

void main() {
  group('session state', () {
    test('deltas append to the message they name', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'assistant_message_started', 'message_id': 'm1'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '你'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '好'});

      expect(state.transcript.single.text, '你好');
    });

    test('an approval the host no longer has is taken off the screen', () {
      // The host restarted while this approval was on screen. Its answer would
      // now resolve nothing — the turn that asked died with the process — so a
      // snapshot without it must clear the card rather than leave a button that
      // silently does nothing. Pinned in Rust as well, by
      // `leveler-app/tests/pending_approval_restart.rs`.
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'approval_requested',
        'request': {
          'id': 'a1',
          'tool': 'run_command',
          'summary': '删除 scratch.txt',
          'command': 'rm scratch.txt',
          'risks': const [],
        },
      });
      expect(state.approvals.keys, ['a1']);

      state.applySnapshot({
        'status': 'idle',
        'messages': const [],
        'pending_interactions': const [],
      });

      expect(state.approvals, isEmpty);
    });

    test('a snapshot replaces the transcript rather than adding to it', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'assistant_message_started', 'message_id': 'm1'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '半句'});

      state.applySnapshot({
        'status': 'idle',
        'messages': [
          {'id': 'm1', 'role': 'assistant', 'text': '完整的一句'},
        ],
        'pending_interactions': const [],
      });

      // Applying both would show the assistant saying it twice.
      expect(state.transcript.length, 1);
      expect(state.transcript.single.text, '完整的一句');
    });

    test('a retry clears the transient message instead of appending to it', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'assistant_message_started', 'message_id': 'm1'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '错的开头'});
      state.applyEvent({'type': 'assistant_attempt_reset', 'message_id': 'm1'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '对的开头'});

      expect(state.transcript.single.text, '对的开头');
    });

    test('an approval resolved by anyone disappears from this phone too', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'approval_requested',
        'request': {'id': 'a1', 'tool': 'run_command', 'summary': 'rm -rf build'},
      });
      expect(state.approvals, contains('a1'));

      // The desktop answered, or the host's timeout did.
      state.applyEvent({'type': 'approval_resolved', 'id': 'a1'});
      expect(state.approvals, isEmpty);
    });

    test('an unknown event is counted, never rendered, and does not force a resync', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'something_this_build_never_heard_of', 'payload': 1});

      expect(state.transcript, isEmpty, reason: 'nothing unreadable may appear as content');
      expect(state.unknownEvents['something_this_build_never_heard_of'], 1);
      // This assertion used to demand the opposite, and the opposite is what
      // shipped: an ordinary turn carries kinds this build has no use for, and
      // asking for a snapshot after each of them left the phone permanently
      // showing "resynchronising". Staleness now comes only from a lagged
      // subscription or an explicit resync_required.
      expect(state.needsResync, isFalse);
    });

    test('a kind we know but do not render is not counted as unknown', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'token_usage', 'input_tokens': 1, 'output_tokens': 2});
      state.applyEvent({'type': 'turn_progress'});

      expect(state.unknownEvents, isEmpty,
          reason: '"chose not to show" and "never heard of" are different states');
      expect(state.transcript, isEmpty);
      expect(state.timeline, isEmpty);
    });

    test('decision-changing runtime events are not silently dropped', () {
      // Token counts may stay quiet. Sub-agents, thinking, verification,
      // completion, and attachments may not: those are why a phone exists.
      const payloads = <Map<String, dynamic>>[
        {
          'type': 'user_message_added',
          'message': {'id': 'u1', 'role': 'user', 'text': 'hi'},
        },
        {
          'type': 'assistant_message_started',
          'message_id': 'm1',
        },
        {
          'type': 'tool_call_started',
          'id': 't1',
          'name': 'read_file',
          'arguments': '{"path":"lib/a.rs"}',
        },
        {
          'type': 'plan_updated',
          'plan': {
            'steps': [
              {'description': 'one', 'status': 'done'},
              {'description': 'two', 'status': 'pending'},
            ],
          },
        },
        {
          'type': 'attachment_added',
          'attachment': {
            'id': 'a1',
            'kind': 'text_file',
            'name': 'out.md',
            'mime_type': 'text/markdown',
            'size_bytes': 12,
            'sha256': 'aa',
          },
        },
        {
          'type': 'approval_requested',
          'request': {'id': 'r1', 'tool': 'run_command', 'summary': 'rm'},
        },
        {'type': 'turn_completed'},
        {
          'type': 'reasoning_delta',
          'delta': '先看测试',
        },
        {
          'type': 'sub_agent_updated',
          'id': 'c1',
          'nickname': 'explorer',
          'role': 'explorer',
          'done': false,
          'ok': false,
          'detail': '查 Trait',
        },
        {
          'type': 'verification_updated',
          'verification': {
            'checks': [
              {'name': 'cargo test', 'status': 'passed'},
            ],
            'passed': true,
          },
        },
        {
          'type': 'session_completed',
          'report': {
            'files_changed': 2,
            'added': 10,
            'removed': 1,
            'checks_passed': 1,
            'checks_total': 1,
            'success': true,
          },
        },
        {
          'type': 'diff_updated',
          'diff': {
            'files': [
              {'path': 'a.rs', 'added': 3, 'removed': 1},
            ],
          },
        },
      ];

      for (final event in payloads) {
        final state = SessionState('s1');
        state.applyEvent(event);
        final type = event['type'] as String;
        expect(state.unknownEvents, isEmpty, reason: '$type counted as unknown');
        expect(
          state.timeline.isNotEmpty || state.approvals.isNotEmpty,
          isTrue,
          reason: '$type was dropped',
        );
      }
    });

    test('sub-agent progress updates the same row instead of flooding the timeline', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'sub_agent_updated',
        'id': 'c1',
        'nickname': 'worker',
        'role': 'worker',
        'done': false,
        'ok': false,
        'detail': '改协议',
      });
      state.applyEvent({
        'type': 'sub_agent_progress',
        'id': 'c1',
        'active': true,
        'input_tokens': 10,
        'output_tokens': 2,
        'cached_input_tokens': 0,
      });
      state.applyEvent({
        'type': 'sub_agent_activity',
        'id': 'c1',
        'phase': 'tool_started',
        'tool': 'read_file',
        'preview': 'lib.rs',
        'is_error': false,
      });
      state.applyEvent({
        'type': 'sub_agent_updated',
        'id': 'c1',
        'nickname': 'worker',
        'role': 'worker',
        'done': true,
        'ok': true,
        'detail': '改完了',
      });

      expect(state.timeline.where((item) => item.kind == TimelineKind.subAgent), hasLength(1));
      expect(state.timeline.single.ok, isTrue);
      expect(state.timeline.single.detail, contains('改完了'));
    });

    test('reasoning_delta accumulates on one thinking row, not the transcript', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'reasoning_delta', 'delta': '先'});
      state.applyEvent({'type': 'reasoning_delta', 'delta': '看测试'});

      expect(state.transcript, isEmpty);
      expect(state.timeline.single.kind, TimelineKind.thinking);
      expect(state.timeline.single.detail, '先看测试');
    });

    test('a local steer is recorded as a user line and not doubled if the host echoes it', () {
      final state = SessionState('s1')..status = 'running';
      state.noteLocalUser('保持 API 兼容，不改库');
      state.applyEvent({
        'type': 'user_message_added',
        'message': {'id': 'u9', 'role': 'user', 'text': '保持 API 兼容，不改库'},
      });

      expect(state.timeline.where((item) => item.kind == TimelineKind.user), hasLength(1));
    });

    test('tool arguments prefer a path over raw JSON', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'tool_call_started',
        'id': 't1',
        'name': 'read_file',
        'arguments': '{"path":"lib/a.rs"}',
      });
      expect(state.timeline.first.detail, 'lib/a.rs');
    });

    test('plan progress counts done steps', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'plan_updated',
        'plan': {
          'steps': [
            {'description': '查 Trait', 'status': 'done'},
            {'description': '改协议', 'status': 'running'},
            {'description': '跑测试', 'status': 'pending'},
          ],
        },
      });
      expect(state.planSteps, 3);
      expect(state.planDone, 1);
      expect(state.timeline.single.detail, contains('查 Trait'));
    });

    test('tool calls land on the timeline, not only in the activity spinner', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'tool_call_started',
        'id': 't1',
        'name': 'read_file',
        'arguments': '{"path":"lib/a.rs"}',
      });
      state.applyEvent({
        'type': 'tool_call_completed',
        'id': 't1',
        'ok': true,
        'preview': 'fn a() {}',
      });

      expect(state.timeline.map((item) => item.kind), [
        TimelineKind.tool,
        TimelineKind.toolResult,
      ]);
      expect(state.timeline.first.title, '读取文件');
      expect(state.transcript, isEmpty);
    });

    test('plan_updated is a timeline row rather than an unknown event', () {
      final state = SessionState('s1');
      state.applyEvent({
        'type': 'plan_updated',
        'plan': {
          'steps': [
            {'title': 'one'},
            {'title': 'two'},
          ],
        },
      });

      expect(state.unknownEvents, isEmpty);
      expect(state.timeline.single.kind, TimelineKind.plan);
      expect(state.sawPlan, isTrue);
    });

    test('a finished turn inserts a status row', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'turn_completed'});
      expect(state.timeline.single.kind, TimelineKind.status);
      expect(state.timeline.single.title, '回合完成');
      expect(state.status, 'idle');
    });

    test('a delta for a message we never saw start is kept but marked suspect', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm9', 'delta': '孤儿'});

      expect(state.transcript.single.text, '孤儿');
      expect(state.needsResync, isTrue);
    });
  });

  group('wire', () {
    test('a downstream frame decodes to the kind it names', () {
      // `utf8.encode`, not `codeUnits`: the wire carries UTF-8 bytes, and
      // `codeUnits` gives UTF-16 units — identical for ASCII, wrong the moment
      // a payload contains anything else, which every Chinese label does.
      final event = DownstreamMessage.decode(
        utf8.encode('{"type":"event","event":{"type":"agent_activity","label":"跑测试"}}'),
      );
      expect(event, isA<RuntimeEventMessage>());
      expect((event as RuntimeEventMessage).kind, 'agent_activity');

      final unknown = DownstreamMessage.decode(utf8.encode('{"type":"from_the_future"}'));
      expect(unknown, isA<UnknownDownstream>());
    });

    test('an upstream deliver carries the id the ack will echo', () {
      final message = DeliverMessage(
        commandId: 'cmd-1',
        sessionId: 's1',
        command: const {'type': 'submit_message'},
      );
      expect(message.toJson()['command_id'], 'cmd-1');
      expect(message.toJson()['type'], 'deliver');
    });
  });

  group('ids', () {
    test('the charset excludes what would break a canonical string', () {
      expect(isValidId('dev_phone.1:2-3'), isTrue);
      expect(isValidId('rpc:0a1b'), isTrue);
      // The separator itself, and anything that is not in the allowed set.
      expect(isValidId('dev|phone'), isFalse);
      expect(isValidId('dev phone'), isFalse);
      expect(isValidId(''), isFalse);
      expect(isValidId('d' * 65), isFalse);
    });
  });

  group('pairing payload', () {
    test('a complete payload parses and drops a trailing slash', () {
      final payload = PairingQrPayload.parse(
        '{"runtime_id":"rt_abc","runtime_pubkey":"AAA","relay_url":"https://relay.example/",'
        '"pairing_secret":"s3cret"}',
      );
      expect(payload.runtimeId, 'rt_abc');
      expect(payload.relayUrl, 'https://relay.example');
    });

    test('a payload missing a field is refused, not half-accepted', () {
      expect(
        () => PairingQrPayload.parse('{"runtime_id":"rt_abc"}'),
        throwsA(isA<FormatException>()),
      );
    });

    test('an illegal runtime id is refused at the moment of trust', () {
      expect(
        () => PairingQrPayload.parse(
          '{"runtime_id":"rt|evil","runtime_pubkey":"AAA","relay_url":"https://r",'
          '"pairing_secret":"s"}',
        ),
        throwsA(isA<FormatException>()),
      );
    });

    test('a non-http relay address is refused', () {
      expect(
        () => PairingQrPayload.parse(
          '{"runtime_id":"rt_abc","runtime_pubkey":"AAA","relay_url":"file:///etc",'
          '"pairing_secret":"s"}',
        ),
        throwsA(isA<FormatException>()),
      );
    });
  });
}
