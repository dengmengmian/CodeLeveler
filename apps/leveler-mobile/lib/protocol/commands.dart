/// The commands this app is allowed to send.
///
/// The host has the authoritative gate — it refuses anything not on its
/// allowlist, and it would refuse these too if the pairing were `observe`. This
/// file is the second, weaker half of that: a phone should not offer a button
/// whose command it knows will be refused, and should never construct one the
/// user did not ask for.
///
/// Two absences are deliberate, not oversights:
///
/// - **No `ApproveAlways`.** It writes a standing rule into the repository that
///   outlives the session and the pairing. The host refuses it; the app does
///   not show the button at all, so nobody wonders why it failed.
/// - **No session deletion, renaming, forking or checkpoint restore.** They are
///   destructive or history-rewriting and have no remote confirmation UX yet.
///
/// Only what a screen actually sends lives here. The host's allowlist is longer,
/// and adding builders for the rest would be API nobody calls — the gate that
/// matters is on the host either way.
library;

/// The decisions a remote client may make on an approval.
enum ApprovalChoice {
  approveOnce('approve_once'),
  approveSession('approve_session'),
  deny('deny');

  const ApprovalChoice(this.wire);
  final String wire;

  String get label => switch (this) {
        ApprovalChoice.approveOnce => '允许一次',
        ApprovalChoice.approveSession => '本次会话内允许',
        ApprovalChoice.deny => '拒绝',
      };
}

/// Builders for the allowed commands. Each returns the `ClientCommand` JSON the
/// host's protocol defines; the shapes come from `schemas/client_command.schema.json`.
class Commands {
  const Commands._();

  static Map<String, dynamic> submitMessage({
    required String sessionId,
    required String content,
    List<String> attachments = const [],
  }) =>
      {
        'type': 'submit_message',
        'session_id': sessionId,
        'content': content,
        'attachments': attachments,
      };

  static Map<String, dynamic> cancelCurrentTurn(String sessionId) =>
      {'type': 'cancel_current_turn', 'session_id': sessionId};

  static Map<String, dynamic> approvalDecision({
    required String requestId,
    required ApprovalChoice decision,
  }) =>
      {'type': 'approval_decision', 'request_id': requestId, 'decision': decision.wire};

  /// An empty answer means "skip", which is why the field is not optional.
  static Map<String, dynamic> answerClarification({
    required String requestId,
    required String answer,
  }) =>
      {'type': 'answer_clarification', 'request_id': requestId, 'answer': answer};

  static Map<String, dynamic> requestSessionList() => {'type': 'request_session_list'};

  static Map<String, dynamic> openSession(String sessionId) =>
      {'type': 'open_session', 'session_id': sessionId};
}
