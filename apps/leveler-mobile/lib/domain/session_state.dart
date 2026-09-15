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

import 'dart:convert';

import 'package:flutter/foundation.dart';

import 'artifact.dart';
import 'task_status.dart';

/// One line of the conversation (kept so snapshot goldens and Markdown
/// rendering stay stable while the timeline is dual-written).
class TranscriptEntry {
  TranscriptEntry({required this.id, required this.role, required this.text});
  final String id;
  final String role;
  String text;
}

/// One row on the Agent timeline. Not a second EventLog — a UI projection of
/// `RuntimeEvent` kinds the chat bubbles used to drop.
enum TimelineKind {
  user,
  assistant,
  tool,
  toolResult,
  plan,
  attachment,
  approval,
  status,
  notice,
  thinking,
  subAgent,
  verification,
  diff,
}

class TimelineItem {
  TimelineItem({
    required this.id,
    required this.kind,
    this.title = '',
    this.detail = '',
    this.ok,
  });

  final String id;
  final TimelineKind kind;
  String title;
  String detail;
  bool? ok;
}

/// One delegated child, as the runtime recorded it.
///
/// Every field is a fact from the host — the snapshot's `children` or a typed
/// live event. The status line is built from `state`, `outcome` and `stop`;
/// nothing here reads a child's prose to decide how it ended.
class ChildAgent {
  ChildAgent(this.id);

  final String id;
  String nickname = '';
  String role = '';
  String? profileId;

  /// The declarative agent this child was spawned from (`security-reviewer`)
  /// and where that definition lives. `null` for a built-in role spawn and
  /// for children recorded before agents existed.
  String? agentName;
  String? agentSource;
  bool readOnly = false;
  String purpose = '';

  /// What the child is called in a row: its agent, else its role.
  String get label => agentName ?? role;

  void _takeAgent(Object? raw) {
    if (raw is! Map<String, dynamic>) return;
    agentName = raw['name'] as String? ?? agentName;
    agentSource = raw['source'] as String? ?? agentSource;
  }

  /// `running`, `interrupted` or `settled`. Only `settled` is final.
  String state = 'running';
  bool ok = false;
  String? outcome;
  String? stop;
  String? summary;
  bool? background;
  List<String> scope = const [];
  int resumes = 0;
  int inputTokens = 0;
  int outputTokens = 0;

  /// Unknown, not zero, when no request carried a price.
  int? costUsdMicros;

  /// The child's latest tool step while it runs.
  String recentStep = '';

  bool cancelRequested = false;

  bool get isOpen => state != 'settled';

  /// Only a running child has anything to stop, and only once.
  bool get canCancel => state == 'running' && !cancelRequested;

  String get displayName => nickname.isEmpty ? id : nickname;

  String get statusLabel {
    switch (state) {
      case 'running':
        return '运行中';
      case 'interrupted':
        return '已中断';
    }
    final result = switch (outcome) {
      'completed_with_findings' => '已完成 · 有发现',
      'completed_no_findings' => '已完成 · 无发现',
      'incomplete_partial' => '部分结果',
      'incomplete_no_result' => '无结果',
      _ => null,
    };
    // Settled before outcomes were typed: the ok bit is all the record has.
    if (result == null) return ok ? '已完成' : '失败';
    final ending = switch (stop) {
      'budget' => '预算耗尽',
      'cancelled' => '已取消',
      'lost' => '已丢失',
      'failed' => '失败',
      'incomplete' => '未完成',
      _ => null,
    };
    return ending == null ? result : '$result · $ending';
  }

  void _settle({required bool ok, String? outcome, String? stop, String? summary}) {
    state = 'settled';
    this.ok = ok;
    this.outcome = outcome;
    this.stop = stop;
    if (summary != null && summary.isNotEmpty) this.summary = summary;
  }

  static ChildAgent fromSnapshot(Map<String, dynamic> json) {
    final child = ChildAgent(json['id'] as String? ?? '')
      ..nickname = json['nickname'] as String? ?? ''
      ..role = json['role'] as String? ?? ''
      ..profileId = json['profile_id'] as String?
      .._takeAgent(json['agent'])
      ..readOnly = json['read_only'] as bool? ?? false
      ..purpose = json['purpose'] as String? ?? ''
      ..state = json['state'] as String? ?? 'running'
      ..ok = json['ok'] as bool? ?? false
      ..outcome = json['outcome'] as String?
      ..stop = json['stop'] as String?
      ..summary = json['summary'] as String?
      ..background = json['background'] as bool?
      ..scope = (json['scope'] as List<dynamic>? ?? const []).map((path) => '$path').toList()
      ..resumes = (json['resumes'] as num?)?.toInt() ?? 0
      ..inputTokens = (json['input_tokens'] as num?)?.toInt() ?? 0
      ..outputTokens = (json['output_tokens'] as num?)?.toInt() ?? 0
      ..costUsdMicros = (json['cost_usd_micros'] as num?)?.toInt();
    return child;
  }
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

  /// What the host is asking for, in this app's own words.
  ///
  /// The `summary` the runtime sends is one English sentence written for every
  /// front-end at once — most often just `<tool> requested by the model`, which
  /// says nothing the tool name does not, in a language this UI is not in.
  /// The structured fields are data rather than prose, so the sentence is built
  /// here where the language is known.
  String get ask => switch (tool) {
        'run_command' || 'shell_command' => '电脑要运行一条命令',
        'apply_patch' || 'replace' || 'write_file' => '电脑要修改文件',
        'checkpoint' => '电脑要创建一个检查点',
        '' => '电脑要执行一个操作',
        _ => '电脑要使用工具 $tool',
      };

  /// The host's own sentence, when it carries more than its default phrasing.
  String? get hostNote {
    final trimmed = summary.trim();
    if (trimmed.isEmpty) return null;
    if (trimmed == '$tool requested by the model') return null;
    return trimmed;
  }

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
  final List<TimelineItem> timeline = [];
  final List<Artifact> artifacts = [];
  final Map<String, PendingApproval> approvals = {};
  final Map<String, PendingClarification> clarifications = {};

  /// Every child of this session, in the order the phone first learned of it.
  final Map<String, ChildAgent> children = {};

  List<ChildAgent> get openChildren =>
      children.values.where((child) => child.isOpen).toList(growable: false);

  /// `spawn_agent` calls on the wire. An accepted one is shown by its child
  /// row; only a refusal, which has no child, is shown as a tool result.
  final Set<String> _spawnCalls = {};

  String status = 'idle';
  String? activity;
  bool sawPlan = false;
  bool sawTool = false;
  int planSteps = 0;
  int planDone = 0;
  List<String> planTitles = [];

  TaskStatus get taskStatus => deriveTaskStatus(
        status: status,
        hasApproval: approvals.isNotEmpty,
        hasClarification: clarifications.isNotEmpty,
        sawPlan: sawPlan && !sawTool && status == 'running',
      );

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
    final messages = (session['messages'] as List<dynamic>? ?? const []).map((raw) {
      final message = raw as Map<String, dynamic>;
      return TranscriptEntry(
        id: message['id'] as String? ?? '',
        // A notice the runtime wrote into the model's context as a user turn.
        // Its role says `user`; its kind says nobody typed it.
        role: message['kind'] == 'runtime_notice'
            ? 'notice'
            : message['role'] as String? ?? 'assistant',
        // `text`, which is what the type calls it. Reading `content`
        // produced a transcript of empty bubbles that looked like the
        // host had said nothing.
        text: message['text'] as String? ?? '',
      );
    }).toList();
    transcript
      ..clear()
      ..addAll(messages);
    timeline
      ..clear()
      ..addAll(messages.map(_itemFromMessage));
    _restoreChildren(session['children'] as List<dynamic>?);
    artifacts.clear();
    status = session['status'] as String? ?? status;
    goal = session['goal'] as String? ?? goal;
    sawPlan = false;
    sawTool = false;
    planSteps = 0;
    planDone = 0;
    planTitles = [];

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
        final user = TranscriptEntry(
          id: message['id'] as String? ?? '',
          role: 'user',
          text: message['text'] as String? ?? '',
        );
        // A local steer already put this text on the timeline. The host does
        // not usually echo it; if it does (race: turn ended, fell through to
        // submit), do not show the user speaking twice.
        if (!_hasTrailingUser(user.text)) {
          transcript.add(user);
          timeline.add(_itemFromMessage(user));
        }
        // There is no `turn_started` on the wire; work begins when a message
        // lands, and one of the `turn_*` kinds ends it.
        status = 'running';
      case 'assistant_message_started':
        final started = TranscriptEntry(
          id: event['message_id'] as String? ?? '',
          role: 'assistant',
          text: '',
        );
        transcript.add(started);
        timeline.add(_itemFromMessage(started));
      case 'assistant_text_delta':
        final id = event['message_id'] as String? ?? '';
        var entry = _entry(id);
        if (entry == null) {
          // A delta for a message we never saw start: keep the text rather than
          // dropping it, and mark the view as suspect.
          entry = TranscriptEntry(id: id, role: 'assistant', text: '');
          transcript.add(entry);
          timeline.add(_itemFromMessage(entry));
          needsResync = true;
        }
        entry.text += event['delta'] as String? ?? '';
        _timelineById(id)?.detail = entry.text;
      case 'assistant_attempt_reset':
        // A retry: the transient message is replaced, not appended to.
        final id = event['message_id'] as String?;
        if (id != null) {
          _entry(id)?.text = '';
          _timelineById(id)?.detail = '';
        }
        timeline.removeWhere((item) => item.id == 'thinking');
      case 'assistant_message_completed':
        activity = null;
      case 'agent_activity':
        activity = event['label'] as String?;
      case 'tool_call_started':
        sawTool = true;
        final name = event['name'] as String? ?? 'tool';
        activity = name;
        if (name == 'spawn_agent') {
          _spawnCalls.add('${event['id']}');
          break;
        }
        timeline.add(TimelineItem(
          id: '${event['id'] ?? timeline.length}-start',
          kind: TimelineKind.tool,
          title: _toolTitle(name),
          detail: _toolDetail(event['arguments'] as String?),
        ));
      case 'tool_call_completed':
        activity = null;
        final ok = event['ok'] as bool? ?? true;
        if (_spawnCalls.remove('${event['id']}') && ok) break;
        timeline.add(TimelineItem(
          id: '${event['id'] ?? timeline.length}-done',
          kind: TimelineKind.toolResult,
          title: ok ? '完成' : '失败',
          detail: event['preview'] as String? ?? '',
          ok: ok,
        ));
      case 'plan_updated':
        sawPlan = true;
        _rememberPlan(event['plan']);
        timeline.add(TimelineItem(
          id: 'plan-${timeline.length}',
          kind: TimelineKind.plan,
          title: '计划已更新',
          detail: _planDetail(event['plan']),
        ));
      case 'attachment_added':
        final attachment = event['attachment'] as Map<String, dynamic>? ?? const {};
        final artifact = Artifact.fromAttachment(sessionId, attachment);
        artifacts.add(artifact);
        timeline.add(TimelineItem(
          id: artifact.id.isEmpty ? 'att-${timeline.length}' : artifact.id,
          kind: TimelineKind.attachment,
          title: artifact.name,
          detail: '${artifact.type.label} · ${artifact.sizeLabel}',
        ));
      case 'reasoning_delta':
        final delta = event['delta'] as String? ?? '';
        if (delta.isEmpty) break;
        final thinking = _timelineById('thinking');
        if (thinking == null) {
          timeline.add(TimelineItem(
            id: 'thinking',
            kind: TimelineKind.thinking,
            title: '思考中',
            detail: delta,
          ));
        } else {
          thinking.detail += delta;
        }
      case 'sub_agent_updated':
        _upsertSubAgent(event);
      case 'sub_agent_state_changed':
        final child = children[event['id'] as String? ?? ''];
        if (child == null) {
          // A child this view never learned of: the view is incomplete, and
          // inventing a nameless row would not fix that.
          needsResync = true;
        } else if (child.isOpen) {
          child.state = event['state'] as String? ?? child.state;
          _renderChildRow(child);
        }
      case 'sub_agent_progress':
      case 'sub_agent_activity':
        // Heartbeats on the same row. A progress event with no started
        // sub-agent is not a reason to invent one.
        final id = event['id'] as String? ?? '';
        final child = children[id];
        final row = _timelineById('sub-$id');
        if (event['type'] == 'sub_agent_progress') {
          if (child != null) {
            child.inputTokens = (event['input_tokens'] as num?)?.toInt() ?? child.inputTokens;
            child.outputTokens = (event['output_tokens'] as num?)?.toInt() ?? child.outputTokens;
          }
        } else {
          final tool = event['tool'] as String? ?? '';
          final preview = event['preview'] as String? ?? '';
          if (tool.isNotEmpty) {
            final step = preview.isEmpty ? tool : '$tool · $preview';
            child?.recentStep = step;
            if (row != null && (child == null || child.isOpen)) row.detail = step;
          }
        }
      case 'verification_updated':
        timeline.removeWhere((item) => item.id == 'verification');
        timeline.add(TimelineItem(
          id: 'verification',
          kind: TimelineKind.verification,
          title: _verificationTitle(event['verification']),
          detail: _verificationDetail(event['verification']),
          ok: _verificationOk(event['verification']),
        ));
      case 'diff_updated':
        final existing = _timelineById('diff');
        final detail = _diffDetail(event['diff']);
        if (existing == null) {
          timeline.add(TimelineItem(
            id: 'diff',
            kind: TimelineKind.diff,
            title: '工作区变更',
            detail: detail,
          ));
        } else {
          existing.detail = detail;
        }
      case 'session_completed':
        final report = event['report'] as Map<String, dynamic>? ?? const {};
        final success = report['success'] as bool? ?? false;
        status = success ? 'completed' : 'failed';
        activity = null;
        timeline.add(TimelineItem(
          id: 'session-done',
          kind: TimelineKind.status,
          title: success ? '任务完成' : '任务失败',
          detail: _completionDetail(report),
          ok: success,
        ));
      case 'notification':
      case 'warning':
      case 'error':
        // Something the host wanted to say. It is not part of the conversation,
        // so it gets its own row rather than an assistant bubble — but it must
        // not vanish, which is what happened before.
        final text = (event['message'] ?? event['error'] ?? event['detail']) as String?;
        if (text != null && text.isNotEmpty) {
          transcript.add(TranscriptEntry(id: '', role: 'notice', text: text));
          timeline.add(TimelineItem(
            id: 'notice-${timeline.length}',
            kind: TimelineKind.notice,
            title: text,
          ));
        }
      case 'approval_requested':
        final approval = PendingApproval.fromJson(
          event['request'] as Map<String, dynamic>? ?? const {},
        );
        approvals[approval.id] = approval;
        timeline.add(TimelineItem(
          id: approval.id,
          kind: TimelineKind.approval,
          title: '需要审批',
          detail: approval.ask,
        ));
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
      case 'turn_completed_checks_failed':
      case 'turn_failed':
      case 'turn_cancelled':
        status = 'idle';
        activity = null;
        if (needsResync) _snapshotDue = true;
        timeline.add(TimelineItem(
          id: 'turn-${timeline.length}',
          kind: TimelineKind.status,
          title: _turnLabel(event['type'] as String? ?? ''),
        ));
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
  /// Heartbeats and context-window bookkeeping stay here. Anything that changes
  /// a person's decision about the running agent (tools, plans, attachments,
  /// thinking, sub-agents, verification, completion) does *not*.
  ///
  /// Anything handled by the switch above does *not* belong here — a kind that
  /// is both rendered and listed as ignored is a comment that lies.
  static const Set<String> _ignored = {
    'runtime_ready',
    // Per-turn progress counters. The settings screen was listing these as
    // "unrecognised" on every ordinary turn, which is a claim that something is
    // wrong when nothing is.
    'turn_progress',
    'command_progress',
    'token_usage',
    'context_updated',
    'context_compacted',
    'context_expanded',
    'project_rules_loaded',
    'checkpoint_created',
    'attachment_processing_failed',
    'session_list',
    'background_task_started',
    'background_task_exited',
    'memory_list',
    'btw_started',
    'btw_text_delta',
    'btw_completed',
  };

  /// Record a steer the user just sent. The host injects it into the next
  /// round and does not emit `user_message_added` for it.
  void noteLocalUser(String text) {
    final trimmed = text.trim();
    if (trimmed.isEmpty || _hasTrailingUser(trimmed)) return;
    final entry = TranscriptEntry(
      id: 'local-${transcript.length}',
      role: 'user',
      text: trimmed,
    );
    transcript.add(entry);
    timeline.add(_itemFromMessage(entry));
    notifyListeners();
  }

  /// The user asked to stop this child. Kept until its terminal arrives, so
  /// a second tap does not send a second command.
  void noteCancelRequested(String childId) {
    final child = children[childId];
    if (child == null || !child.isOpen) return;
    child.cancelRequested = true;
    notifyListeners();
  }

  bool _snapshotDue = false;

  /// True once when a turn ended while this view was stale. A snapshot taken
  /// mid-turn would miss the answer still streaming, so the request waits for
  /// the turn's end; nothing else asks for one.
  bool takeSnapshotDue() {
    final due = _snapshotDue;
    _snapshotDue = false;
    return due;
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

  TimelineItem? _timelineById(String id) {
    for (final item in timeline.reversed) {
      if (item.id == id) return item;
    }
    return null;
  }

  static TimelineItem _itemFromMessage(TranscriptEntry entry) => switch (entry.role) {
        'user' => TimelineItem(id: entry.id, kind: TimelineKind.user, detail: entry.text),
        'notice' => TimelineItem(
            id: entry.id,
            kind: TimelineKind.notice,
            title: _noticeTitle(entry.text),
            detail: entry.text,
          ),
        _ => TimelineItem(id: entry.id, kind: TimelineKind.assistant, detail: entry.text),
      };

  /// The registered header line a runtime notice opens with, without its
  /// Markdown marker.
  static String _noticeTitle(String text) {
    final first = text.split('\n').first.trim();
    return first.replaceFirst(RegExp(r'^#+\s*'), '');
  }

  static String _toolTitle(String name) => switch (name) {
        'read_file' || 'read' => '读取文件',
        'grep' || 'search' => '搜索',
        'apply_patch' || 'replace' || 'write_file' => '修改文件',
        'run_command' || 'shell_command' => '运行命令',
        'update_plan' => '更新计划',
        'spawn_agent' => '启动子 Agent',
        _ => name,
      };

  static String _toolDetail(String? arguments) {
    if (arguments == null || arguments.isEmpty || arguments == '{}') return '';
    try {
      final decoded = jsonDecode(arguments);
      if (decoded is Map) {
        for (final key in const ['path', 'file', 'query', 'pattern', 'command']) {
          final value = decoded[key];
          if (value is String && value.isNotEmpty) return value;
        }
      }
    } on FormatException {
      // Fall through to the truncated raw arguments.
    }
    return arguments.length > 120 ? '${arguments.substring(0, 117)}…' : arguments;
  }

  void _rememberPlan(Object? plan) {
    if (plan is! Map) return;
    final steps = plan['steps'];
    if (steps is! List) return;
    planSteps = steps.length;
    planDone = steps.where((raw) {
      if (raw is! Map) return false;
      final status = raw['status']?.toString();
      return status == 'done' || status == 'skipped';
    }).length;
    planTitles = steps
        .map((raw) {
          if (raw is Map) {
            return (raw['description'] ?? raw['title'] ?? '').toString();
          }
          return '';
        })
        .where((title) => title.isNotEmpty)
        .toList();
  }

  static String _planDetail(Object? plan) {
    if (plan is Map) {
      final steps = plan['steps'];
      if (steps is List) {
        final titles = steps
            .map((raw) {
              if (raw is Map) {
                return (raw['description'] ?? raw['title'] ?? '').toString();
              }
              return '';
            })
            .where((title) => title.isNotEmpty)
            .toList();
        if (titles.isNotEmpty) return titles.join('\n');
        return '${steps.length} 个步骤';
      }
    }
    return '';
  }

  bool _hasTrailingUser(String text) {
    for (final item in timeline.reversed) {
      if (item.kind == TimelineKind.user) return item.detail == text;
    }
    return false;
  }

  void _upsertSubAgent(Map<String, dynamic> event) {
    final id = event['id'] as String?;
    if (id == null || id.isEmpty) {
      // A child with no id cannot be shown truthfully or stopped; the view
      // is missing something a snapshot will supply.
      needsResync = true;
      return;
    }
    final child = children.putIfAbsent(id, () => ChildAgent(id));
    final done = event['done'] as bool? ?? false;
    if (!done && !child.isOpen) {
      // A settled child is final; a late or replayed start does not reopen it.
      _renderChildRow(child);
      return;
    }
    child
      ..nickname = event['nickname'] as String? ?? child.nickname
      ..role = event['role'] as String? ?? child.role
      ..profileId = event['profile_id'] as String? ?? child.profileId
      .._takeAgent(event['agent'])
      ..readOnly = event['read_only'] as bool? ?? child.readOnly;
    final background = event['background'] as bool?;
    if (background != null) child.background = background;
    final scope = event['scope'] as List<dynamic>?;
    if (scope != null) child.scope = scope.map((path) => '$path').toList();
    final detail = event['detail'] as String? ?? '';
    if (done) {
      child._settle(
        ok: event['ok'] as bool? ?? false,
        outcome: event['outcome'] as String?,
        stop: event['stop'] as String?,
        summary: detail,
      );
    } else {
      child.state = 'running';
      if (detail.isNotEmpty) child.purpose = detail;
    }
    _renderChildRow(child);
  }

  /// Merge the host's durable children into what this view already has.
  ///
  /// The snapshot is older than any event that arrived after it was taken, so
  /// it never reopens a child this view saw settle, and a child started after
  /// it keeps its row.
  void _restoreChildren(List<dynamic>? raw) {
    for (final entry in raw ?? const []) {
      final restored = ChildAgent.fromSnapshot(entry as Map<String, dynamic>);
      final known = children[restored.id];
      if (known != null && !known.isOpen && restored.isOpen) continue;
      if (known != null) {
        restored
          ..recentStep = known.recentStep
          ..cancelRequested = known.cancelRequested;
      }
      children[restored.id] = restored;
    }
    // The snapshot rebuilt the timeline from messages; the children's rows
    // come from the merged record.
    for (final child in children.values) {
      _renderChildRow(child);
    }
  }

  void _renderChildRow(ChildAgent child) {
    final title = child.label.isEmpty
        ? '子 Agent ${child.displayName}'
        : '子 Agent ${child.displayName} · ${child.label}';
    final text = child.isOpen ? child.purpose : (child.summary ?? '');
    final body = text.isEmpty ? child.statusLabel : '${child.statusLabel}\n$text';
    final ok = child.isOpen ? null : child.ok;
    final existing = _timelineById('sub-${child.id}');
    if (existing == null) {
      timeline.add(TimelineItem(
        id: 'sub-${child.id}',
        kind: TimelineKind.subAgent,
        title: title,
        detail: body,
        ok: ok,
      ));
    } else {
      existing
        ..title = title
        ..detail = body
        ..ok = ok;
    }
  }

  static String _verificationTitle(Object? raw) {
    if (raw is Map) {
      final passed = raw['passed'];
      if (passed == true) return '验证通过';
      if (passed == false) return '验证失败';
    }
    return '正在验证';
  }

  static String _verificationDetail(Object? raw) {
    if (raw is! Map) return '';
    final checks = raw['checks'];
    if (checks is! List) return '';
    final passed = checks.where((item) {
      return item is Map && item['status']?.toString() == 'passed';
    }).length;
    return '$passed / ${checks.length} 项';
  }

  static bool? _verificationOk(Object? raw) {
    if (raw is Map) {
      final passed = raw['passed'];
      if (passed is bool) return passed;
    }
    return null;
  }

  static String _diffDetail(Object? raw) {
    if (raw is! Map) return '';
    final files = raw['files'];
    if (files is! List) return '';
    var added = 0;
    var removed = 0;
    for (final file in files) {
      if (file is Map) {
        added += (file['added'] as num?)?.toInt() ?? 0;
        removed += (file['removed'] as num?)?.toInt() ?? 0;
      }
    }
    return '${files.length} 个文件  +$added −$removed';
  }

  static String _completionDetail(Map<String, dynamic> report) {
    final files = report['files_changed'] ?? 0;
    final added = report['added'] ?? 0;
    final removed = report['removed'] ?? 0;
    final passed = report['checks_passed'] ?? 0;
    final total = report['checks_total'] ?? 0;
    return '$files 个文件  +$added −$removed · 验证 $passed / $total';
  }

  static String _turnLabel(String type) => switch (type) {
        'turn_completed' || 'turn_answered' => '回合完成',
        'turn_completed_unverified' => '回合完成（未验证）',
        'turn_completed_checks_failed' => '回合完成（验证未通过）',
        'turn_incomplete' || 'turn_truncated' => '回合未完成',
        'turn_failed' => '回合失败',
        'turn_cancelled' => '已取消',
        _ => '状态更新',
      };
}
