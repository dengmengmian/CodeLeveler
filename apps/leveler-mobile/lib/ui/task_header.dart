/// Compact task facts above the timeline. Not a second session model.
library;

import 'package:flutter/material.dart';

import '../domain/session_state.dart';
import '../domain/task_status.dart';
import 'status_chip.dart';

class TaskHeader extends StatelessWidget {
  const TaskHeader({super.key, required this.session, this.onOpen});
  final SessionState session;
  final VoidCallback? onOpen;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final activity = session.activity;
    final facts = <String>[
      if (session.planSteps > 0) '${session.planDone} / ${session.planSteps}',
      if (session.artifacts.isNotEmpty) '${session.artifacts.length} 个产物',
    ];

    return Material(
      color: theme.colorScheme.surfaceContainerLowest,
      child: InkWell(
        onTap: onOpen,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 10, 16, 10),
          child: Row(
          children: [
            StatusChip(status: session.taskStatus, compact: true),
            const SizedBox(width: 10),
            Expanded(
              child: Text(
                activity ?? session.taskStatus.label,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: theme.textTheme.bodySmall,
              ),
            ),
            if (facts.isNotEmpty)
              Text(
                facts.join(' · '),
                style: theme.textTheme.labelSmall?.copyWith(
                  color: theme.colorScheme.onSurfaceVariant,
                ),
              ),
            if (onOpen != null)
              Icon(Icons.chevron_right, size: 18, color: theme.colorScheme.outline),
          ],
        ),
        ),
      ),
    );
  }
}
