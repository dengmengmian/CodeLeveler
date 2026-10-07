/// Execution Presentation Contract v1 — the App's projection.
///
/// The same pure functions the Web and Desktop surfaces implement, for the
/// remote-control app. Execution presentation contract v1 is the authority;
/// `testdata/execution_presentation/v1/*.json` is the shared oracle this file
/// is checked against (`test/execution_presentation_test.dart`).
///
/// Nothing here reads prose or invents a boundary: the round is the runtime's
/// `model_step`, a batch is an observed overlap, and Final vs Progress is
/// decided by arrival order.
library;

/// The frozen five-way reading of a tool call.
enum ToolStatus { running, ok, failed, cancelled, unknown }

/// Wire status string -> the frozen vocabulary.
ToolStatus toolStatusFromWire(String? wire) => switch (wire) {
      'running' || 'run' => ToolStatus.running,
      'success' || 'done' || 'ok' => ToolStatus.ok,
      'failed' || 'fail' => ToolStatus.failed,
      'cancelled' => ToolStatus.cancelled,
      _ => ToolStatus.unknown,
    };

/// `ok` plus the runtime's stop fact -> the frozen vocabulary. A confirmed stop
/// is cancelled, an unconfirmed one is unknown; neither is ever a failure.
ToolStatus toolStatusFromOutcome(bool ok, String? stop) {
  if (stop == 'confirmed') return ToolStatus.cancelled;
  if (stop == 'unconfirmed') return ToolStatus.unknown;
  return ok ? ToolStatus.ok : ToolStatus.failed;
}

/// One tool call as the contract sees it.
class ToolFact {
  ToolFact({
    required this.id,
    required this.name,
    required this.status,
    required this.seq,
    this.modelStep,
    this.parallel = false,
    this.batch,
    this.preview = '',
    this.answerEffect = AnswerEffect.work,
  });

  final String id;
  final String name;
  ToolStatus status;

  /// Arrival position in the same sequence as [ProjectedMessage.seq].
  final int seq;
  final int? modelStep;
  final bool parallel;
  int? batch;
  String preview;

  /// The runtime's classification of what this call does to the answer.
  ///
  /// The runtime stamps it on every call it projects; the App reads it and
  /// never re-decides it from [name]. [AnswerEffect.work] is the conservative
  /// read when an older peer did not state the fact.
  AnswerEffect answerEffect;
}

/// A burst of calls from one model response.
class ExecutionRoundView {
  ExecutionRoundView({
    required this.modelStep,
    required this.tools,
    required this.status,
    required this.allOk,
    required this.batches,
  });

  final int? modelStep;
  final List<ToolFact> tools;

  /// `running | ok | failed | cancelled | unknown`.
  final String status;

  /// True only when EVERY visible call is [ToolStatus.ok].
  final bool allOk;

  /// Observed concurrent bursts inside this round, in first-seen order.
  final List<List<String>> batches;
}

/// Whether a started call acts on the turn's answer.
///
/// The classification is the RUNTIME's, stated on the wire as the call's
/// `answer_effect` (`leveler_tools::acts_on_answer` is its one owner). This
/// reads that fact — the App must never re-derive it from the tool name, which
/// is what made `FinalAnswer` a second truth source.
enum AnswerEffect {
  work,
  bookkeeping;

  static AnswerEffect parse(Object? raw) =>
      raw == 'bookkeeping' ? AnswerEffect.bookkeeping : AnswerEffect.work;

  bool get actsOnAnswer => this == AnswerEffect.work;
}

const List<String> _runtimeNoteTags = [
  '[execution policy] ',
  '[mutation rejected] ',
  '[note] ',
];

/// A runtime row about HOW a command ran, never why it failed.
bool isRuntimeNote(String line) {
  final trimmed = line.trimRight();
  if (trimmed.startsWith('exit: ')) return true;
  if (trimmed.startsWith('--- ') && trimmed.endsWith(' ---')) return true;
  if (trimmed == '[timed out]') return true;
  if (trimmed.startsWith('[timed out after ') && trimmed.endsWith(']')) return true;
  return _runtimeNoteTags.any(trimmed.startsWith);
}

/// The command's own output, without the runtime's execution rows.
List<String> commandOutputBody(String preview) => preview
    .split('\n')
    .map((line) => line.trimRight())
    .where((line) => line.trim().isNotEmpty && !isRuntimeNote(line))
    .toList(growable: false);

/// The line that says what actually failed, never a runtime note.
String? failureReason(String preview) {
  final lines = commandOutputBody(preview);
  for (final line in lines) {
    final trimmed = line.trim();
    final lower = trimmed.toLowerCase();
    if (lower.startsWith('error') ||
        lower.startsWith('fail') ||
        trimmed.startsWith('✗') ||
        lower.startsWith('panic')) {
      return trimmed;
    }
  }
  return lines.isEmpty ? null : lines.first.trim();
}

/// The preview a row renders.
String? displayPreview(String name, String preview) {
  if (preview.isEmpty) return null;
  if (name != 'run_command' && name != 'shell_command') return preview;
  final body = commandOutputBody(preview).join('\n');
  return body.isEmpty ? null : body;
}

String _activityClass(String name) {
  final n = name.toLowerCase();
  if (RegExp(r'apply_patch|edit|write|patch').hasMatch(n)) return 'edit';
  if (RegExp(r'read|cat|open|view').hasMatch(n)) return 'read';
  if (RegExp(r'search|grep|find|glob|list').hasMatch(n)) return 'search';
  if (RegExp(r'bash|shell|exec|command|run|terminal|cargo|npm|git_(?!diff)').hasMatch(n)) {
    return 'command';
  }
  return 'other';
}

bool _startsNewRound(ExecutionRoundView round, ToolFact tool) {
  if (round.modelStep != null && tool.modelStep != null) {
    return round.modelStep != tool.modelStep;
  }
  if (round.tools.any((t) => t.status == ToolStatus.running)) return false;
  if (round.tools.isEmpty) return false;
  return _activityClass(round.tools.last.name) != _activityClass(tool.name);
}

String roundStatusOf(List<ToolFact> tools) {
  if (tools.any((t) => t.status == ToolStatus.running)) return 'running';
  if (tools.isNotEmpty && tools.every((t) => t.status == ToolStatus.ok)) return 'ok';
  if (tools.any((t) => t.status == ToolStatus.failed)) return 'failed';
  if (tools.any((t) => t.status == ToolStatus.cancelled)) return 'cancelled';
  return 'unknown';
}

List<List<String>> _roundBatches(List<ToolFact> tools) {
  final order = <int>[];
  final batches = <List<String>>[];
  for (final tool in tools) {
    final batch = tool.batch;
    if (batch == null) continue;
    final index = order.indexOf(batch);
    if (index >= 0) {
      batches[index].add(tool.id);
      continue;
    }
    order.add(batch);
    batches.add([tool.id]);
  }
  return batches;
}

bool _roundAllOk(List<ToolFact> tools) =>
    tools.isNotEmpty && tools.every((t) => t.status == ToolStatus.ok);

/// The batch a newly started call joins, or null when it ran alone.
///
/// Observed, never inferred: it joins only when another parallel call is still
/// running.
int? observedBatch(List<ToolFact> existing, {required bool parallel}) {
  if (!parallel) return null;
  final inFlight = existing
      .where((t) => t.status == ToolStatus.running && t.parallel)
      .toList(growable: false);
  if (inFlight.isEmpty) return null;
  for (final tool in inFlight.reversed) {
    if (tool.batch != null) return tool.batch;
  }
  var highest = -1;
  for (final tool in existing) {
    final batch = tool.batch;
    if (batch != null && batch > highest) highest = batch;
  }
  return highest + 1;
}

/// Group a turn's calls into execution rounds.
List<ExecutionRoundView> groupExecutionRounds(List<ToolFact> tools) {
  final rounds = <ExecutionRoundView>[];
  for (final tool in tools) {
    if (rounds.isNotEmpty && !_startsNewRound(rounds.last, tool)) {
      rounds.last.tools.add(tool);
      continue;
    }
    rounds.add(ExecutionRoundView(
      modelStep: tool.modelStep,
      tools: [tool],
      status: 'running',
      allOk: false,
      batches: const [],
    ));
  }
  return [
    for (final round in rounds)
      ExecutionRoundView(
        modelStep: round.modelStep,
        tools: round.tools,
        status: roundStatusOf(round.tools),
        allOk: _roundAllOk(round.tools),
        batches: _roundBatches(round.tools),
      ),
  ];
}

/// Status glyph for a round head. Visual only; the headline carries the fact.
String roundGlyph(ExecutionRoundView round) => switch (round.status) {
      'running' => '●',
      'ok' => '✓',
      'failed' => '✗',
      'cancelled' => '■',
      _ => '◇',
    };

/// A truthful one-line head. All-success is claimed only when every call did.
String roundHeadline(ExecutionRoundView round) {
  final total = round.tools.length;
  return switch (round.status) {
    'running' => '执行中 · $total 项',
    'ok' => round.allOk ? '完成 $total 项' : '$total 项已结束',
    'failed' =>
      '完成 $total 项 · ${round.tools.where((t) => t.status == ToolStatus.failed).length} 项失败',
    'cancelled' => '已停止 · $total 项',
    _ => '结果未知 · $total 项',
  };
}

/// A message as the projection sees it.
class ProjectedMessage {
  ProjectedMessage({
    required this.role,
    required this.text,
    required this.seq,
    this.kind,
    this.btw,
  });

  final String role;
  final String text;
  final int seq;
  final String? kind;
  final String? btw;

  bool get isTurnUser => role == 'user' && btw == null && kind == null;
}

/// One node of the projected turn.
class ProjectedItem {
  ProjectedItem.text(this.kind, this.text, this.seq) : round = null;
  ProjectedItem.round(this.round, this.seq)
      : kind = 'execution_round',
        text = null;

  /// `assistant_text | final_answer | execution_round`.
  final String kind;
  final String? text;
  final ExecutionRoundView? round;
  final int seq;
}

class _Node {
  _Node.assistant(this.text, this.seq) : round = null, demoted = false;
  _Node.round(this.round, this.seq) : text = '', demoted = false;

  String text;
  bool demoted;
  final ExecutionRoundView? round;
  final int seq;
}

/// Project one turn onto the contract's items.
///
/// `turnEnded` is the mechanical moment that decides the FinalAnswer: only a
/// terminal may turn a still-undecided message into the answer.
List<ProjectedItem> projectTurn(
  List<ProjectedMessage> messages,
  List<ToolFact> tools,
  bool turnEnded,
) {
  var startSeq = -1;
  for (final message in messages) {
    if (message.isTurnUser && message.seq > startSeq) startSeq = message.seq;
  }
  final merged = <({int seq, ProjectedMessage? message, ToolFact? tool})>[
    for (final message in messages)
      if (message.seq > startSeq) (seq: message.seq, message: message, tool: null),
    for (final tool in tools) (seq: tool.seq, message: null, tool: tool),
  ]..sort((a, b) => a.seq.compareTo(b.seq));

  final nodes = <_Node>[];
  for (final entry in merged) {
    final message = entry.message;
    if (message != null) {
      if (message.btw != null) continue;
      // Only the model's public assistant content is AssistantText.
      if (message.role != 'assistant' && message.kind != 'compaction_summary') continue;
      if (message.text.trim().isEmpty) continue;
      nodes.add(_Node.assistant(message.text, entry.seq));
      continue;
    }
    final tool = entry.tool!;
    if (tool.answerEffect.actsOnAnswer) {
      for (final node in nodes) {
        if (node.round == null) node.demoted = true;
      }
    }
    if (nodes.isNotEmpty &&
        nodes.last.round != null &&
        !_startsNewRound(nodes.last.round!, tool)) {
      nodes.last.round!.tools.add(tool);
    } else {
      nodes.add(_Node.round(
        ExecutionRoundView(
          modelStep: tool.modelStep,
          tools: [tool],
          status: 'running',
          allOk: false,
          batches: const [],
        ),
        entry.seq,
      ));
    }
  }

  final items = <ProjectedItem>[];
  for (final node in nodes) {
    final round = node.round;
    if (round == null) {
      items.add(ProjectedItem.text(
        turnEnded && !node.demoted ? 'final_answer' : 'assistant_text',
        node.text,
        node.seq,
      ));
      continue;
    }
    items.add(ProjectedItem.round(
      ExecutionRoundView(
        modelStep: round.modelStep,
        tools: round.tools,
        status: roundStatusOf(round.tools),
        allOk: _roundAllOk(round.tools),
        batches: _roundBatches(round.tools),
      ),
      node.seq,
    ));
  }
  return items;
}

/// The turn's committed answer, or null when it ended without one.
String? committedFinalAnswer(List<ProjectedMessage> messages, List<ToolFact> tools) {
  String? answer;
  for (final item in projectTurn(messages, tools, true)) {
    if (item.kind == 'final_answer') answer = item.text;
  }
  return answer;
}

/// The turn terminal, keeping §I9: a Completed/Answered turn with no committed
/// answer must not read as a green completion.
String? turnTerminalFromEvent(
  String type,
  List<ProjectedMessage> messages,
  List<ToolFact> tools,
) {
  if (type == 'turn_completed' || type == 'turn_answered') {
    if (committedFinalAnswer(messages, tools) == null) return 'no_final_answer';
    return type == 'turn_answered' ? 'answered' : 'completed';
  }
  return switch (type) {
    'turn_completed_with_warnings' => 'completed_with_warnings',
    'turn_truncated' => 'truncated',
    'turn_incomplete' || 'turn_completed_unverified' || 'turn_completed_checks_failed' =>
      'incomplete',
    'turn_failed' => 'failed',
    'turn_cancelled' => 'cancelled',
    _ => null,
  };
}
