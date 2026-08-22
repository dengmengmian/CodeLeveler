/// The commands this phone is willing to construct.
///
/// The host's schema is the authority (`schemas/client_command.schema.json`).
/// These tests exist so a composer change cannot quietly send `submit_message`
/// while a turn is already running — that is a follow-up, not a steer.
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/protocol/commands.dart';

void main() {
  test('steer_current_turn matches the host schema', () {
    expect(
      Commands.steerCurrentTurn(sessionId: 's1', content: '保持 API 兼容，不改库'),
      {
        'type': 'steer_current_turn',
        'session_id': 's1',
        'content': '保持 API 兼容，不改库',
      },
    );
  });

  test('a running turn sends a steer, not a queued follow-up', () {
    expect(
      Commands.forComposer(sessionId: 's1', content: '先别改数据库', turnRunning: true)['type'],
      'steer_current_turn',
    );
  });

  test('an idle composer still submits a new turn', () {
    expect(
      Commands.forComposer(sessionId: 's1', content: '下一步写测试', turnRunning: false)['type'],
      'submit_message',
    );
  });
}
