/// One chip for a coding task's status, used on Home, lists, and the timeline.
library;

import 'package:flutter/material.dart';

import '../domain/task_status.dart';

class StatusChip extends StatelessWidget {
  const StatusChip({super.key, required this.status, this.compact = false});

  final TaskStatus status;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final (Color fg, Color bg) = switch (status) {
      TaskStatus.running || TaskStatus.planning => (
          theme.colorScheme.primary,
          theme.colorScheme.primary.withValues(alpha: 0.12),
        ),
      TaskStatus.waitingApproval || TaskStatus.waitingInput => (
          theme.colorScheme.tertiary,
          theme.colorScheme.tertiary.withValues(alpha: 0.14),
        ),
      TaskStatus.completed => (
          const Color(0xFF2FA36B),
          const Color(0xFF2FA36B).withValues(alpha: 0.12),
        ),
      TaskStatus.failed => (
          theme.colorScheme.error,
          theme.colorScheme.error.withValues(alpha: 0.12),
        ),
      TaskStatus.created => (
          theme.colorScheme.onSurfaceVariant,
          theme.colorScheme.surfaceContainerHighest,
        ),
    };

    return Container(
      padding: EdgeInsets.symmetric(horizontal: compact ? 8 : 10, vertical: compact ? 2 : 4),
      decoration: BoxDecoration(
        color: bg,
        borderRadius: BorderRadius.circular(6),
      ),
      child: Text(
        status.label,
        style: theme.textTheme.labelSmall?.copyWith(
          color: fg,
          fontWeight: FontWeight.w600,
          fontSize: compact ? 11 : 12,
        ),
      ),
    );
  }
}
