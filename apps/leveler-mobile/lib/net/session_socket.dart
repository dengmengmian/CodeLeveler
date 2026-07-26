/// The live session stream: signed frames in both directions over a socket the
/// relay can see but not author.
///
/// Every inbound frame is verified before it becomes anything the UI can show.
/// A frame that fails is surfaced as a `SocketFailure` rather than dropped —
/// silence would look identical to an idle session, and "your host said
/// nothing" is a very different thing from "someone tampered with this".
library;

import 'dart:async';
import 'dart:convert';

import 'package:cryptography/cryptography.dart';
// The IO channel, not the cross-platform one: only this variant can send the
// Authorization header, and the design forbids putting the token in the query
// string, where it would land in access logs and proxy history.
import 'package:web_socket_channel/io.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import '../protocol/envelope.dart';
import '../protocol/wire.dart';

/// What comes out of the socket.
sealed class SocketEvent {}

class SocketMessage extends SocketEvent {
  SocketMessage(this.message);
  final DownstreamMessage message;
}

/// A frame that could not be trusted, or a transport that ended.
class SocketFailure extends SocketEvent {
  SocketFailure(this.code, this.detail);
  final String code;
  final String detail;
}

class SessionSocket {
  SessionSocket._(this._channel, this._events);

  final WebSocketChannel _channel;
  final Stream<SocketEvent> _events;

  Stream<SocketEvent> get events => _events;

  /// Open a stream for one project on one host.
  ///
  /// The token travels in a header, never a query string: query strings land in
  /// access logs and proxy history.
  static Future<SessionSocket> connect({
    required String relayUrl,
    required String hostId,
    required String projectId,
    required String accessToken,
    required String deviceId,
    required SimplePublicKey runtimePublicKey,
    DateTime Function()? clock,
  }) async {
    final wsUrl = relayUrl.startsWith('https://')
        ? relayUrl.replaceFirst('https://', 'wss://')
        : relayUrl.replaceFirst('http://', 'ws://');
    final uri = Uri.parse('$wsUrl/v1/hosts/$hostId/session?project_id=$projectId');
    final channel = IOWebSocketChannel.connect(
      uri,
      headers: {'Authorization': 'Bearer $accessToken'},
      // Subprotocol negotiation, so a version mismatch fails at the handshake
      // rather than at the first frame nobody can parse.
      protocols: const ['leveler.session.v1'],
    );
    await channel.ready;

    final now = clock ?? DateTime.now;
    final controller = StreamController<SocketEvent>();
    channel.stream.listen(
      (raw) async {
        try {
          final decoded = jsonDecode(raw as String);
          if (decoded is! Map<String, dynamic>) {
            controller.add(SocketFailure('invalid_frame', 'not a JSON object'));
            return;
          }
          final envelope = SignedEnvelope.fromJson(decoded);
          if (envelope.sender != FrameSender.runtime) {
            // Only the host speaks downstream. A frame claiming to be from a
            // device is a relay inventing traffic.
            controller.add(SocketFailure('invalid_frame', 'downstream frame not from the runtime'));
            return;
          }
          final payload = await verifyEnvelope(
            envelope,
            expectedRecipientId: deviceId,
            publicKey: runtimePublicKey,
            now: now(),
          );
          controller.add(SocketMessage(DownstreamMessage.decode(payload)));
        } on EnvelopeException catch (error) {
          controller.add(SocketFailure(error.code, '$error'));
        } catch (error) {
          controller.add(SocketFailure('invalid_frame', '$error'));
        }
      },
      onError: (Object error) => controller.add(SocketFailure('stream_closed', '$error')),
      onDone: () {
        controller.add(SocketFailure('stream_closed', 'the host closed this stream'));
        controller.close();
      },
    );

    return SessionSocket._(channel, controller.stream);
  }

  /// Send one already-signed frame.
  void send(SignedEnvelope frame) {
    _channel.sink.add(jsonEncode(frame.toJson()));
  }

  Future<void> close() => _channel.sink.close();
}

/// The per-stream sequence number.
///
/// Monotonic within a stream, as the envelope spec requires. A fresh stream
/// starts over, which is why the host tracks it per `(sender, stream)` rather
/// than per device.
class SeqCounter {
  int _next = 1;
  int take() => _next++;
}
