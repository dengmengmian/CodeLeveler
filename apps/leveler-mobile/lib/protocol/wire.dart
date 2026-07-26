/// The session messages carried inside an envelope's payload.
///
/// Mirrors `leveler-session-wire`. Two rules from the design shape this file:
///
/// - **Upstream is closed.** The app sends only what it can name; a command the
///   host would refuse should not leave the phone at all.
/// - **Downstream is open.** An event kind this build has never heard of is
///   kept as raw JSON and ignored for rendering, then a snapshot is requested.
///   Failing the connection instead would make every host upgrade break every
///   older phone; silently dropping it would leave a hole in the transcript
///   that looks like fact.
library;

import 'dart:convert';

/// What the phone sends.
sealed class UpstreamMessage {
  Map<String, dynamic> toJson();

  List<int> encode() => utf8.encode(jsonEncode(toJson()));
}

class DeliverMessage extends UpstreamMessage {
  DeliverMessage({
    required this.commandId,
    required this.sessionId,
    required this.command,
  });

  /// Chosen by the phone and echoed in the ack, so a queued message can be
  /// matched to its answer — and so a retry after a lost connection is the
  /// same command rather than a second one.
  final String commandId;
  final String sessionId;
  final Map<String, dynamic> command;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'deliver',
        'command_id': commandId,
        'session_id': sessionId,
        'command': command,
      };
}

class SnapshotRequest extends UpstreamMessage {
  SnapshotRequest(this.sessionId);
  final String sessionId;

  @override
  Map<String, dynamic> toJson() => {'type': 'snapshot', 'session_id': sessionId};
}

/// What the host sends.
sealed class DownstreamMessage {
  static DownstreamMessage decode(List<int> payload) {
    final decoded = jsonDecode(utf8.decode(payload));
    if (decoded is! Map<String, dynamic>) {
      return UnknownDownstream({'raw': '$decoded'});
    }
    switch (decoded['type']) {
      case 'event':
        return RuntimeEventMessage(decoded['event'] as Map<String, dynamic>? ?? const {});
      case 'snapshot':
        return SnapshotMessage(decoded['session'] as Map<String, dynamic>? ?? const {});
      case 'ack':
        return AckMessage(decoded['command_id'] as String? ?? '');
      case 'error':
        return ErrorMessage(
          code: decoded['code'] as String? ?? 'unknown',
          message: decoded['message'] as String? ?? '',
          commandId: decoded['command_id'] as String?,
        );
      case 'resync_required':
        return ResyncRequired(decoded['session_id'] as String? ?? '');
      case 'project_status':
        return ProjectStatusMessage(
          path: decoded['path'] as String? ?? '',
          status: decoded['status'] as String? ?? 'offline',
        );
      default:
        return UnknownDownstream(decoded);
    }
  }
}

class RuntimeEventMessage extends DownstreamMessage {
  RuntimeEventMessage(this.event);
  final Map<String, dynamic> event;
  String get kind => event['type'] as String? ?? 'unknown';
}

class SnapshotMessage extends DownstreamMessage {
  SnapshotMessage(this.session);
  final Map<String, dynamic> session;
}

class AckMessage extends DownstreamMessage {
  AckMessage(this.commandId);
  final String commandId;
}

class ErrorMessage extends DownstreamMessage {
  ErrorMessage({required this.code, required this.message, this.commandId});
  final String code;
  final String message;
  final String? commandId;
}

class ResyncRequired extends DownstreamMessage {
  ResyncRequired(this.sessionId);
  final String sessionId;
}

class ProjectStatusMessage extends DownstreamMessage {
  ProjectStatusMessage({required this.path, required this.status});
  final String path;
  final String status;
}

/// A frame this build does not understand. Kept whole so a log can show what
/// arrived, and treated as a reason to resynchronise rather than as an error.
class UnknownDownstream extends DownstreamMessage {
  UnknownDownstream(this.raw);
  final Map<String, dynamic> raw;
  String get kind => raw['type'] as String? ?? 'unknown';
}
