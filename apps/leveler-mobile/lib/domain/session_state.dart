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

  /// What this session was started to do. Shown while the transcript is still
  /// empty, so a session the user just created does not look like a blank
  /// screen that swallowed their goal.
  String goal = '';

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
              // `text`, which is what the type calls it. Reading `content`
              // produced a transcript of empty bubbles that looked like the
              // host had said nothing.
              text: message['text'] as String? ?? '',
            );
          },
        ),
      );
    status = session['status'] as String? ?? status;
    goal = session['goal'] as String? ?? goal;

    approvals.clear();
    clarifications.clear();
    for (final raw in session['pending_interactions'] as List<dynamic>? ?? const []) {
      final interaction = raw as Map<String, dynamic>;
      // The request is nested under `request`, not flattened into the wrapper.
      final request = interaction['request'] as Map<String, dynamic>? ?? const {};
      switch (interaction['type']) {
        case 'approval':
          final approval = PendingApproval.fromJson(request);
          approvals[approval.id] = approval;
        case 'clarification':
          final id = request['id'] as String? ?? '';
          clarifications[id] = PendingClarification(
            id: id,
            question: request['question'] as String? ?? '',
            options: (request['options'] as List<dynamic>? ?? const [])
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
          text: message['text'] as String? ?? '',
        ));
        // There is no `turn_started` on the wire; work begins when a message
        // lands, and one of the `turn_*` kinds ends it.
        status = 'running';
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
      case 'tool_call_started':
        // The only sign a phone has that work is happening between messages.
        // Dropping these left a minute of "运行中…" with nothing under it.
        activity = event['name'] as String?;
      case 'tool_call_completed':
        activity = null;
      case 'notification':
      case 'warning':
      case 'error':
        // Something the host wanted to say. It is not part of the conversation,
        // so it gets its own row rather than an assistant bubble — but it must
        // not vanish, which is what happened before.
        final text = (event['message'] ?? event['error'] ?? event['detail']) as String?;
        if (text != null && text.isNotEmpty) {
          transcript.add(TranscriptEntry(id: '', role: 'notice', text: text));
        }
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
      // Every way a turn can end.
      case 'turn_completed':
      case 'turn_answered':
      case 'turn_truncated':
      case 'turn_incomplete':
      case 'turn_completed_unverified':
      case 'turn_failed':
      case 'turn_cancelled':
        status = 'idle';
        activity = null;
      default:
        if (!_ignored.contains(event['type'])) {
          final kind = event['type'] as String? ?? 'unknown';
          unknownEvents.update(kind, (count) => count + 1, ifAbsent: () => 1);
        }
    }
    notifyListeners();
  }

  /// Kinds this screen deliberately does not render.
  ///
  /// Listing them is the difference between "we chose not to show this" and "we
  /// have never heard of this". They used to fall through to the unknown branch,
  /// which asked the host for a fresh snapshot — after *every* token-usage
  /// event — and left the phone permanently displaying "resynchronising".
  ///
  /// Several deserve a place in the UI eventually (tool activity, plans, diffs).
  /// Until they have one, being ignored on purpose is the honest state.
  static const Set<String> _ignored = {
    'runtime_ready',
    'reasoning_delta',
    'token_usage',
    'context_updated',
    'command_progress',
    'notification',
    'project_rules_loaded',
    'tool_call_started',
    'tool_call_completed',
    'plan_updated',
    'verification_updated',
    'diff_updated',
    'checkpoint_created',
    'attachment_added',
    'attachment_processing_failed',
    'session_list',
    'session_completed',
    'sub_agent_updated',
    'sub_agent_progress',
    'sub_agent_activity',
    'background_task_started',
    'background_task_exited',
    'memory_list',
    'btw_started',
    'btw_text_delta',
    'btw_completed',
  };

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
