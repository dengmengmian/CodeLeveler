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
        role: message['role'] as String? ?? 'assistant',
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
        timeline.add(TimelineItem(
          id: '${event['id'] ?? timeline.length}-start',
          kind: TimelineKind.tool,
          title: _toolTitle(name),
          detail: _toolDetail(event['arguments'] as String?),
        ));
      case 'tool_call_completed':
        activity = null;
        timeline.add(TimelineItem(
          id: '${event['id'] ?? timeline.length}-done',
          kind: TimelineKind.toolResult,
          title: (event['ok'] as bool? ?? true) ? '完成' : '失败',
          detail: event['preview'] as String? ?? '',
          ok: event['ok'] as bool? ?? true,
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
      case 'sub_agent_progress':
      case 'sub_agent_activity':
        // Heartbeats on the same row. A progress event with no started
        // sub-agent is not a reason to invent one.
        final id = event['id'] as String? ?? '';
        final row = _timelineById('sub-$id');
        if (row != null && event['type'] == 'sub_agent_activity') {
          final tool = event['tool'] as String? ?? '';
          final preview = event['preview'] as String? ?? '';
          if (tool.isNotEmpty) {
            row.detail = preview.isEmpty ? tool : '$tool · $preview';
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
      case 'turn_failed':
      case 'turn_cancelled':
        status = 'idle';
        activity = null;
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

  static TimelineItem _itemFromMessage(TranscriptEntry entry) => TimelineItem(
        id: entry.id,
        kind: entry.role == 'user' ? TimelineKind.user : TimelineKind.assistant,
        detail: entry.text,
      );

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
    final id = event['id'] as String? ?? 'sub-${timeline.length}';
    final nickname = event['nickname'] as String? ?? '';
    final role = event['role'] as String? ?? '';
    final done = event['done'] as bool? ?? false;
    final ok = event['ok'] as bool? ?? false;
    final detail = event['detail'] as String? ?? '';
    final who = nickname.isEmpty ? id : nickname;
    final title = role.isEmpty ? '子 Agent $who' : '子 Agent $who · $role';
    final status = done ? (ok ? '已完成' : '失败') : '运行中';
    final body = detail.isEmpty ? status : '$status\n$detail';
    final existing = _timelineById('sub-$id');
    if (existing == null) {
      timeline.add(TimelineItem(
        id: 'sub-$id',
        kind: TimelineKind.subAgent,
        title: title,
        detail: body,
        ok: done ? ok : null,
      ));
    } else {
      existing.title = title;
      existing.detail = body;
      existing.ok = done ? ok : null;
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
        'turn_incomplete' || 'turn_truncated' => '回合未完成',
        'turn_failed' => '回合失败',
        'turn_cancelled' => '已取消',
        _ => '状态更新',
      };
}
