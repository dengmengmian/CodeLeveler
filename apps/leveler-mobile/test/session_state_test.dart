/// What the transcript must and must not do with a stream of events.
library;

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

    test('a snapshot replaces the transcript rather than adding to it', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'assistant_message_started', 'message_id': 'm1'});
      state.applyEvent({'type': 'assistant_text_delta', 'message_id': 'm1', 'delta': '半句'});

      state.applySnapshot({
        'status': 'idle',
        'messages': [
          {'id': 'm1', 'role': 'assistant', 'content': '完整的一句'},
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

    test('an unknown event is counted and forces a resync, never rendered', () {
      final state = SessionState('s1');
      state.applyEvent({'type': 'something_this_build_never_heard_of', 'payload': 1});

      expect(state.transcript, isEmpty, reason: 'nothing unreadable may appear as content');
      expect(state.unknownEvents['something_this_build_never_heard_of'], 1);
      expect(state.needsResync, isTrue, reason: 'it may have changed state we do show');
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
      final event = DownstreamMessage.decode(
        '{"type":"event","event":{"type":"agent_activity","label":"跑测试"}}'.codeUnits,
      );
      expect(event, isA<RuntimeEventMessage>());
      expect((event as RuntimeEventMessage).kind, 'agent_activity');

      final unknown = DownstreamMessage.decode('{"type":"from_the_future"}'.codeUnits);
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
