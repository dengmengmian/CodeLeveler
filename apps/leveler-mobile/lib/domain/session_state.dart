/// Turning a stream of runtime events into something a screen can show.
///
/// The rules that matter are about *not* showing things:
///
/// - A snapshot replaces the transcript; deltas after it are appended. Applying
///   both would double the assistant's words.
/// - An event kind this build does not know is counted and ignored, not
///   rendered as a mystery row and not treated as fatal.
/// - Anything that failed verification never reaches this class at all.
library;

import 'package:flutter/foundation.dart';

/// One line of the conversation.
class TranscriptEntry {
  TranscriptEntry({required this.id, required this.role, required this.text});
  final String id;
  final String role;
  String text;
}

/// An approval the host is waiting on.
class PendingApproval {
  PendingApproval({
    required this.id,
    required this.tool,
    required this.summary,
    this.command,
    this.risks = const [],
  });

  final String id;
  final String tool;
  final String summary;
  final String? command;
  final List<String> risks;

  static PendingApproval fromJson(Map<String, dynamic> json) => PendingApproval(
        id: json['id'] as String? ?? '',
        tool: json['tool'] as String? ?? '',
        summary: json['summary'] as String? ?? '',
        command: json['command'] as String?,
        risks: (json['risks'] as List<dynamic>? ?? const [])
            .map((risk) => '$risk')
            .toList(growable: false),
      );
}

/// A clarification the agent is waiting on. An empty answer means "skip".
class PendingClarification {
  PendingClarification({required this.id, required this.question, this.options = const []});
  final String id;
  final String question;
  final List<String> options;
}

/// The state of one session as this phone understands it.
class SessionState extends ChangeNotifier {
  SessionState(this.sessionId);

  final String sessionId;

  final List<TranscriptEntry> transcript = [];
  final Map<String, PendingApproval> approvals = {};
  final Map<String, PendingClarification> clarifications = {};

  String status = 'idle';
  String? activity;

  /// Event kinds this build does not know. Surfaced in settings rather than
  /// hidden: a user seeing "3 unknown events" has a reason to update the app,
  /// where silence would just look like missing output.
  final Map<String, int> unknownEvents = {};

  /// True when the phone's view may be incomplete and a snapshot is due.
  bool needsResync = false;

  void applySnapshot(Map<String, dynamic> session) {
    transcript
      ..clear()
      ..addAll(
        (session['messages'] as List<dynamic>? ?? const []).map(
          (raw) {
            final message = raw as Map<String, dynamic>;
            return TranscriptEntry(
              id: message['id'] as String? ?? '',
              role: message['role'] as String? ?? 'assistant',
              text: message['content'] as String? ?? '',
            );
          },
        ),
      );
    status = session['status'] as String? ?? status;

    approvals.clear();
    clarifications.clear();
    for (final raw in session['pending_interactions'] as List<dynamic>? ?? const []) {
      final interaction = raw as Map<String, dynamic>;
      switch (interaction['type']) {
        case 'approval':
          final approval = PendingApproval.fromJson(interaction);
          approvals[approval.id] = approval;
        case 'clarification':
          final id = interaction['id'] as String? ?? '';
          clarifications[id] = PendingClarification(
            id: id,
            question: interaction['question'] as String? ?? '',
            options: (interaction['options'] as List<dynamic>? ?? const [])
                .map((option) => '$option')
                .toList(growable: false),
          );
      }
    }

    needsResync = false;
    notifyListeners();
  }

  void applyEvent(Map<String, dynamic> event) {
    switch (event['type']) {
      case 'user_message_added':
        final message = event['message'] as Map<String, dynamic>? ?? const {};
        transcript.add(TranscriptEntry(
          id: message['id'] as String? ?? '',
          role: 'user',
          text: message['content'] as String? ?? '',
        ));
      case 'assistant_message_started':
        transcript.add(TranscriptEntry(
          id: event['message_id'] as String? ?? '',
          role: 'assistant',
          text: '',
        ));
      case 'assistant_text_delta':
        final id = event['message_id'] as String? ?? '';
        var entry = _entry(id);
        if (entry == null) {
          // A delta for a message we never saw start: keep the text rather than
          // dropping it, and mark the view as suspect.
          entry = TranscriptEntry(id: id, role: 'assistant', text: '');
          transcript.add(entry);
          needsResync = true;
        }
        entry.text += event['delta'] as String? ?? '';
      case 'assistant_attempt_reset':
        // A retry: the transient message is replaced, not appended to.
        final id = event['message_id'] as String?;
        if (id != null) _entry(id)?.text = '';
      case 'assistant_message_completed':
        activity = null;
      case 'agent_activity':
        activity = event['label'] as String?;
      case 'approval_requested':
        final approval = PendingApproval.fromJson(
          event['request'] as Map<String, dynamic>? ?? const {},
        );
        approvals[approval.id] = approval;
      case 'approval_resolved':
        // Any client may have answered — or the host's own timeout did.
        approvals.remove(event['id'] as String? ?? '');
      case 'clarification_requested':
        final request = event['request'] as Map<String, dynamic>? ?? const {};
        final id = request['id'] as String? ?? '';
        clarifications[id] = PendingClarification(
          id: id,
          question: request['question'] as String? ?? '',
          options: (request['options'] as List<dynamic>? ?? const [])
              .map((option) => '$option')
              .toList(growable: false),
        );
      case 'clarification_resolved':
        clarifications.remove(event['id'] as String? ?? '');
      case 'session_opened':
      case 'session_updated':
        final session = event['session'] as Map<String, dynamic>?;
        if (session != null && session['id'] == sessionId) {
          status = session['status'] as String? ?? status;
        }
      case 'turn_started':
        status = 'running';
      case 'turn_completed':
      case 'turn_completed_unverified':
      case 'turn_failed':
        status = 'idle';
        activity = null;
      default:
        final kind = event['type'] as String? ?? 'unknown';
        unknownEvents.update(kind, (count) => count + 1, ifAbsent: () => 1);
        // A kind we cannot render may have changed state we do render.
        needsResync = true;
    }
    notifyListeners();
  }

  void markResyncRequired() {
    needsResync = true;
    notifyListeners();
  }

  TranscriptEntry? _entry(String id) {
    for (final entry in transcript.reversed) {
      if (entry.id == id) return entry;
    }
    return null;
  }
}
