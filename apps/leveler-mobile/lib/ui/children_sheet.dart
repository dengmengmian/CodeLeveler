/// The session's delegated children, from the runtime's own record.
///
/// Open children come first. A stop button appears only on a running child
/// and only for a pairing that may send commands: an interrupted or settled
/// child has nothing to stop, and a read-only pairing would be refused.
library;

import 'package:flutter/material.dart';

import '../domain/session_state.dart';

class ChildrenSheet extends StatelessWidget {
  const ChildrenSheet({
    super.key,
    required this.session,
    required this.canControl,
    required this.onCancel,
  });

  final SessionState session;
  final bool canControl;
  final void Function(String childId) onCancel;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: session,
      builder: (context, _) {
        final ordered = [
          ...session.children.values.where((child) => child.isOpen),
          ...session.children.values.where((child) => !child.isOpen),
        ];
        return ListView.separated(
          shrinkWrap: true,
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 24),
          itemCount: ordered.length,
          separatorBuilder: (_, __) => const Divider(height: 20),
          itemBuilder: (context, index) => _ChildTile(
            child: ordered[index],
            canControl: canControl,
            onCancel: onCancel,
          ),
        );
      },
    );
  }
}

class _ChildTile extends StatelessWidget {
  const _ChildTile({required this.child, required this.canControl, required this.onCancel});

  final ChildAgent child;
  final bool canControl;
  final void Function(String childId) onCancel;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant);
    final text = child.isOpen
        ? (child.recentStep.isEmpty ? child.purpose : child.recentStep)
        : (child.summary ?? '');
    final bounds = [
      if (child.agentSource != null) '${child.agentSource} Agent',
      child.readOnly ? '只读' : '可写',
      if (child.background != null) child.background! ? '后台' : '前台',
      if (child.scope.isNotEmpty) '范围 ${child.scope.join(', ')}',
      if (child.resumes > 0) '续跑 ${child.resumes} 次',
    ].join(' · ');
    final cost = child.costUsdMicros == null
        ? '费用未知'
        : '\$${(child.costUsdMicros! / 1000000).toStringAsFixed(6)}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Expanded(
              child: Text(
                child.label.isEmpty ? child.displayName : '${child.displayName} · ${child.label}',
                style: theme.textTheme.titleSmall,
              ),
            ),
            if (canControl && child.canCancel)
              ConstrainedBox(
                constraints: const BoxConstraints(minHeight: 44),
                child: OutlinedButton(
                  onPressed: () => onCancel(child.id),
                  child: const Text('停止'),
                ),
              )
            else if (canControl && child.cancelRequested && child.isOpen)
              ConstrainedBox(
                constraints: const BoxConstraints(minHeight: 44),
                child: const OutlinedButton(onPressed: null, child: Text('停止中…')),
              ),
          ],
        ),
        Text(
          child.statusLabel,
          style: theme.textTheme.bodyMedium?.copyWith(
            color: !child.isOpen && !child.ok ? theme.colorScheme.error : null,
          ),
        ),
        if (text.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(text, maxLines: 4, overflow: TextOverflow.ellipsis),
          ),
        const SizedBox(height: 4),
        Text(bounds, style: muted),
        Text('↑${child.inputTokens} ↓${child.outputTokens} · $cost', style: muted),
      ],
    );
  }
}
