/// The conversation, and the approvals that interrupt it.
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
import '../domain/session_state.dart';
import 'common.dart';
import '../protocol/commands.dart';

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
  }

  @override
  Widget build(BuildContext context) {
    final controller = widget.controller;
    final session = controller.session;
    if (session == null) return const SizedBox.shrink();

    return ListenableBuilder(
      listenable: session,
      builder: (context, _) {
        final approval = session.approvals.values.firstOrNull;
        final clarification = session.clarifications.values.firstOrNull;

        return Scaffold(
          appBar: AppBar(
            leading: IconButton(
              icon: const Icon(Icons.arrow_back),
              onPressed: () => controller.closeSession(),
            ),
            title: Text(session.status == 'running' ? '运行中…' : '会话'),
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
              if (session.needsResync)
                const Material(
                  child: Padding(
                    padding: EdgeInsets.symmetric(horizontal: 16, vertical: 8),
                    child: Text('正在与开发机重新同步…', style: TextStyle(fontSize: 12)),
                  ),
                ),
              Expanded(
                child: ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.all(12),
                  itemCount: session.transcript.length,
                  itemBuilder: (context, index) => _Bubble(entry: session.transcript[index]),
                ),
              ),
              if (session.activity != null)
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 4),
                  child: Row(
                    children: [
                      const SizedBox(
                        width: 12, height: 12, child: CircularProgressIndicator(strokeWidth: 2)),
                      const SizedBox(width: 8),
                      Expanded(child: Text(session.activity!, style: const TextStyle(fontSize: 12))),
                    ],
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

class _Bubble extends StatelessWidget {
  const _Bubble({required this.entry});
  final TranscriptEntry entry;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final mine = entry.role == 'user';
    return Align(
      alignment: mine ? Alignment.centerRight : Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.symmetric(vertical: 4),
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
        constraints: BoxConstraints(maxWidth: MediaQuery.of(context).size.width * 0.82),
        decoration: BoxDecoration(
          color: mine
              ? theme.colorScheme.primaryContainer
              : theme.colorScheme.surfaceContainerHighest,
          borderRadius: BorderRadius.circular(14),
        ),
        child: SelectableText(entry.text),
      ),
    );
  }
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
            Text('需要你批准', style: theme.textTheme.titleMedium),
            const SizedBox(height: 8),
            Text(approval.summary),
            if (approval.command != null) ...[
              const SizedBox(height: 8),
              SelectableText(
                approval.command!,
                style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
              ),
            ],
            for (final risk in approval.risks)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Text('• $risk', style: theme.textTheme.bodySmall),
              ),
            const SizedBox(height: 12),
            Wrap(
              spacing: 8,
              children: [
                // Minimum 44pt targets: this is the screen where a mis-tap is
                // most expensive.
                for (final choice in ApprovalChoice.values)
                  ConstrainedBox(
                    constraints: const BoxConstraints(minHeight: 44),
                    child: choice == ApprovalChoice.deny
                        ? OutlinedButton(
                            onPressed: () => onDecision(choice),
                            child: Text(choice.label),
                          )
                        : FilledButton(
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
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 12, 12),
      child: Row(
        children: [
          Expanded(
            child: TextField(
              controller: input,
              minLines: 1,
              maxLines: 5,
              textInputAction: TextInputAction.send,
              onSubmitted: (_) => onSend(),
              decoration: const InputDecoration(
                hintText: '说点什么…',
                border: OutlineInputBorder(),
                isDense: true,
              ),
            ),
          ),
          const SizedBox(width: 8),
          IconButton.filled(
            onPressed: onSend,
            icon: const Icon(Icons.arrow_upward),
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
