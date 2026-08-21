/// The agent timeline, and the approvals that interrupt it.
///
/// Two things this screen deliberately does not offer:
///
/// - **"Always allow".** It writes a standing rule into the repository that
///   outlives both the session and this pairing. The host refuses it from a
///   remote client, so showing the button would only produce a failure the user
///   cannot explain.
/// - **A composer, when the pairing is read-only.** Same reasoning: the command
///   would be refused, and a disabled field with a reason is kinder than a
///   message that vanishes.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import '../domain/artifact.dart';
import '../domain/session_state.dart';
import 'artifact_preview.dart';
import 'common.dart';
import '../protocol/commands.dart';
import 'task_detail_screen.dart';
import 'task_header.dart';
import 'timeline.dart';

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key, required this.controller});
  final AppController controller;

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final TextEditingController _input = TextEditingController();
  final ScrollController _scroll = ScrollController();

  @override
  void dispose() {
    _input.dispose();
    _scroll.dispose();
    super.dispose();
  }

  Future<void> _send() async {
    final text = _input.text;
    if (text.trim().isEmpty) return;
    _input.clear();
    await widget.controller.submit(text);
    _followTail();
  }

  /// Keep the newest message in view while a reply streams in.
  ///
  /// The list is reversed, so "the bottom" is offset zero. Only follows when
  /// the user is already near it: yanking someone back down while they are
  /// reading earlier output is worse than making them scroll.
  void _followTail() {
    if (!_scroll.hasClients) return;
    if (_scroll.position.pixels > 240) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!_scroll.hasClients) return;
      _scroll.animateTo(
        0,
        duration: const Duration(milliseconds: 180),
        curve: Curves.easeOut,
      );
    });
  }

  @override
  Widget build(BuildContext context) {
    final controller = widget.controller;
    final session = controller.session;
    if (session == null) return const SizedBox.shrink();

    return ListenableBuilder(
      listenable: session,
      builder: (context, _) {
        // New content arrived; follow it if the user was at the tail.
        _followTail();
        final approval = session.approvals.values.firstOrNull;
        final clarification = session.clarifications.values.firstOrNull;

        return Scaffold(
          appBar: AppBar(
            leading: IconButton(
              icon: const Icon(Icons.arrow_back),
              onPressed: () => controller.closeSession(),
            ),
            title: Text(
              session.goal.isEmpty ? '任务' : session.goal,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            actions: [
              if (session.status == 'running')
                IconButton(
                  icon: const Icon(Icons.stop_circle_outlined),
                  tooltip: '取消当前回合',
                  onPressed: controller.cancelTurn,
                ),
              settingsButton(context, controller),
            ],
          ),
          body: Column(
            children: [
              StatusBanner(controller: controller),
              TaskHeader(
                session: session,
                onOpen: () => Navigator.of(context).push(
                  MaterialPageRoute<void>(
                    builder: (_) => TaskDetailScreen(controller: controller),
                  ),
                ),
              ),
              if (session.needsResync)
                const Material(
                  child: Padding(
                    padding: EdgeInsets.symmetric(horizontal: 16, vertical: 8),
                    child: Text('正在与开发机重新同步…', style: TextStyle(fontSize: 12)),
                  ),
                ),
              Expanded(
                // Selection lives here rather than inside each bubble: a
                // per-bubble SelectableText claims the vertical drag for
                // selecting, so the list would not scroll under a mouse at all.
                child: SelectionArea(
                  child: session.timeline.isEmpty
                      ? _EmptySession(goal: session.goal)
                      // Reversed, so a short timeline sits against the composer
                      // and "the newest row" is offset zero.
                      : ListView.builder(
                          controller: _scroll,
                          reverse: true,
                          padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
                          itemCount: session.timeline.length,
                          itemBuilder: (context, index) {
                            final item = session.timeline[session.timeline.length - 1 - index];
                            final artifact = item.kind == TimelineKind.attachment
                                ? _artifactById(session, item.id)
                                : null;
                            return TimelineRow(
                              item: item,
                              artifact: artifact,
                              onOpenArtifact: artifact == null
                                  ? null
                                  : () => _openArtifact(context, artifact, controller),
                            );
                          },
                        ),
                ),
              ),
              if (clarification != null)
                _ClarificationCard(
                  clarification: clarification,
                  onAnswer: (answer) =>
                      controller.answerClarification(clarification.id, answer),
                ),
              if (approval != null)
                _ApprovalCard(
                  approval: approval,
                  onDecision: (choice) => controller.answerApproval(approval.id, choice),
                ),
              SafeArea(child: _Composer(controller: controller, input: _input, onSend: _send)),
            ],
          ),
        );
      },
    );
  }
}

/// What a session shows before anyone has said anything.
///
/// It used to show nothing at all: a user who had just typed a goal arrived at
/// a blank screen with no sign the goal had been received, or that this was a
/// session rather than a failure.
class _EmptySession extends StatelessWidget {
  const _EmptySession({required this.goal});

  /// Only to decide whether there is anything to say about *why* this session
  /// exists. The goal itself is in the app bar; printing it here as well made a
  /// nearly empty screen repeat itself.
  final String goal;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Center(
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 40),
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Icon(Icons.task_alt_outlined, size: 36, color: theme.colorScheme.outline),
            const SizedBox(height: 16),
            Text(
              goal.isEmpty ? '还没有开始' : '任务已经建立',
              style: theme.textTheme.titleSmall,
            ),
            const SizedBox(height: 6),
            Text(
              '电脑会开始执行。你可以在下方追加要求。',
              textAlign: TextAlign.center,
              style: theme.textTheme.bodySmall?.copyWith(
                color: theme.colorScheme.onSurfaceVariant,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

Artifact? _artifactById(SessionState session, String id) {
  for (final artifact in session.artifacts) {
    if (artifact.id == id) return artifact;
  }
  return null;
}

void _openArtifact(BuildContext context, Artifact artifact, AppController controller) {
  Navigator.of(context).push(
    MaterialPageRoute<void>(
      builder: (_) => ArtifactPreviewPage(
        artifact: artifact,
        onFetch: () => controller.fetchAttachment(artifact),
      ),
    ),
  );
}

class _ApprovalCard extends StatelessWidget {
  const _ApprovalCard({required this.approval, required this.onDecision});
  final PendingApproval approval;
  final void Function(ApprovalChoice) onDecision;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      margin: const EdgeInsets.all(12),
      color: theme.colorScheme.errorContainer,
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(Icons.pan_tool_outlined, size: 18, color: theme.colorScheme.onErrorContainer),
                const SizedBox(width: 8),
                Text('需要你批准', style: theme.textTheme.titleMedium),
              ],
            ),
            const SizedBox(height: 10),
            Text(approval.ask, style: theme.textTheme.bodyLarge),
            if (approval.command != null) ...[
              const SizedBox(height: 10),
              Container(
                width: double.infinity,
                padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
                decoration: BoxDecoration(
                  color: theme.colorScheme.surface,
                  borderRadius: BorderRadius.circular(10),
                ),
                child: SelectableText(
                  approval.command!,
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
                ),
              ),
            ],
            if (approval.hostNote != null) ...[
              const SizedBox(height: 10),
              Text(approval.hostNote!, style: theme.textTheme.bodySmall),
            ],
            for (final risk in approval.risks)
              Padding(
                padding: const EdgeInsets.only(top: 6),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Icon(Icons.warning_amber_rounded,
                        size: 15, color: theme.colorScheme.onErrorContainer),
                    const SizedBox(width: 6),
                    Expanded(child: Text(risk, style: theme.textTheme.bodySmall)),
                  ],
                ),
              ),
            const SizedBox(height: 14),
            // Weight follows how much each answer gives away, not how likely
            // it is to be tapped. Two filled buttons for "allow" against a
            // ghost "deny" made the wide answer the easy one on the screen
            // where a mis-tap costs the most. Only the single allow is filled;
            // the session-wide one and the refusal are equals beside it.
            //
            // Minimum 44pt targets throughout, for the same reason.
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (final choice in ApprovalChoice.values)
                  ConstrainedBox(
                    constraints: const BoxConstraints(minHeight: 44),
                    child: choice == ApprovalChoice.approveOnce
                        ? FilledButton(
                            onPressed: () => onDecision(choice),
                            child: Text(choice.label),
                          )
                        : OutlinedButton(
                            onPressed: () => onDecision(choice),
                            child: Text(choice.label),
                          ),
                  ),
              ],
            ),
            const SizedBox(height: 8),
            Text(
              '没有「始终允许」：那会在仓库里写下一条比这次配对更长久的规则，电脑端会拒绝。',
              style: theme.textTheme.bodySmall,
            ),
          ],
        ),
      ),
    );
  }
}

class _ClarificationCard extends StatefulWidget {
  const _ClarificationCard({required this.clarification, required this.onAnswer});
  final PendingClarification clarification;
  final void Function(String) onAnswer;

  @override
  State<_ClarificationCard> createState() => _ClarificationCardState();
}

class _ClarificationCardState extends State<_ClarificationCard> {
  final TextEditingController _answer = TextEditingController();

  @override
  void dispose() {
    _answer.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      margin: const EdgeInsets.all(12),
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('需要你确认一下', style: theme.textTheme.titleMedium),
            const SizedBox(height: 8),
            Text(widget.clarification.question),
            const SizedBox(height: 8),
            if (widget.clarification.options.isNotEmpty)
              Wrap(
                spacing: 8,
                children: [
                  for (final option in widget.clarification.options)
                    OutlinedButton(
                      onPressed: () => widget.onAnswer(option),
                      child: Text(option),
                    ),
                ],
              ),
            TextField(
              controller: _answer,
              decoration: InputDecoration(
                hintText: '或者直接回答（留空表示跳过）',
                suffixIcon: IconButton(
                  icon: const Icon(Icons.send),
                  onPressed: () => widget.onAnswer(_answer.text),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _Composer extends StatelessWidget {
  const _Composer({required this.controller, required this.input, required this.onSend});
  final AppController controller;
  final TextEditingController input;
  final VoidCallback onSend;

  @override
  Widget build(BuildContext context) {
    if (controller.isObserveOnly) {
      return const Padding(
        padding: EdgeInsets.all(16),
        child: Text('只读配对：可以查看，不能发送指令。', style: TextStyle(fontSize: 12)),
      );
    }
    final theme = Theme.of(context);
    return Container(
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
      decoration: BoxDecoration(
        color: theme.colorScheme.surface,
        border: Border(top: BorderSide(color: theme.colorScheme.outlineVariant, width: 0.5)),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.end,
        children: [
          Expanded(
            child: TextField(
              controller: input,
              minLines: 1,
              maxLines: 5,
              textInputAction: TextInputAction.send,
              onSubmitted: (_) => onSend(),
              decoration: InputDecoration(
                hintText: controller.session?.status == 'running'
                    ? '干预当前回合（立刻生效）'
                    : '追加要求…',
                isDense: true,
              ),
            ),
          ),
          const SizedBox(width: 10),
          IconButton.filled(
            onPressed: onSend,
            icon: const Icon(Icons.arrow_upward, size: 20),
            tooltip: '发送',
          ),
        ],
      ),
    );
  }
}

extension _FirstOrNull<T> on Iterable<T> {
  T? get firstOrNull => isEmpty ? null : first;
}
