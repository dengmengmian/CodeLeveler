/// The sessions inside one project.
///
/// The list arrives as a runtime *event* on the project's stream rather than as
/// a reply to a request, so this screen shows "loading" until one turns up and
/// says plainly when none does — a blank list and a broken stream look the same
/// otherwise.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import '../domain/task_status.dart';
import 'common.dart';
import 'status_chip.dart';

class SessionsScreen extends StatelessWidget {
  const SessionsScreen({super.key, required this.controller, required this.projectName});

  final AppController controller;
  final String projectName;

  Future<void> _newSession(BuildContext context) async {
    final goal = await showDialog<String>(
      context: context,
      builder: (context) => const _GoalDialog(),
    );
    if (goal == null || goal.isEmpty) return;
    try {
      await controller.startSession(controller.currentProjectId!, goal: goal);
    } catch (error) {
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text('$error')));
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        leading: IconButton(
          icon: const Icon(Icons.arrow_back),
          onPressed: controller.closeProject,
        ),
        title: Text(projectName),
        actions: [settingsButton(context, controller)],
      ),
      floatingActionButton: controller.isObserveOnly
          ? null
          : FloatingActionButton.extended(
              onPressed: () => _newSession(context),
              icon: const Icon(Icons.add),
              label: const Text('新任务'),
            ),
      body: Column(
        children: [
          StatusBanner(controller: controller),
          if (controller.sessionsLoading) const LinearProgressIndicator(),
          Expanded(
            child: controller.sessions.isEmpty
                ? Center(
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Icon(
                          controller.sessionsLoading
                              ? Icons.hourglass_empty
                              : Icons.task_alt_outlined,
                          size: 40,
                          color: Theme.of(context).colorScheme.outline,
                        ),
                        const SizedBox(height: 14),
                        Text(
                          controller.sessionsLoading ? '正在读取任务…' : '这个项目还没有任务',
                          style: Theme.of(context).textTheme.titleSmall,
                        ),
                        if (!controller.sessionsLoading && !controller.isObserveOnly) ...[
                          const SizedBox(height: 6),
                          Text(
                            '点右下角开一个',
                            style: Theme.of(context).textTheme.bodySmall?.copyWith(
                                  color: Theme.of(context).colorScheme.onSurfaceVariant,
                                ),
                          ),
                        ],
                      ],
                    ),
                  )
                : ListView.separated(
                    itemCount: controller.sessions.length,
                    separatorBuilder: (_, __) => const Divider(height: 1),
                    itemBuilder: (context, index) {
                      final session = controller.sessions[index];
                      final theme = Theme.of(context);
                      return ListTile(
                        title: Text(
                          session.goal.isEmpty ? '(无目标)' : session.goal,
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis,
                          style: theme.textTheme.bodyLarge,
                        ),
                        subtitle: Padding(
                          padding: const EdgeInsets.only(top: 4),
                          child: Row(
                            children: [
                              StatusChip(status: session.taskStatus, compact: true),
                              const SizedBox(width: 8),
                              Flexible(
                                child: Text(
                                  session.taskStatus == TaskStatus.running
                                      ? '运行中'
                                      : _when(session.updatedAt),
                                  style: theme.textTheme.bodySmall?.copyWith(
                                    color: session.taskStatus == TaskStatus.running
                                        ? theme.colorScheme.primary
                                        : theme.colorScheme.onSurfaceVariant,
                                  ),
                                ),
                              ),
                            ],
                          ),
                        ),
                        trailing: Icon(Icons.chevron_right,
                            size: 20, color: theme.colorScheme.outline),
                        onTap: () => controller.openSession(
                          controller.currentProjectId!,
                          session.id,
                        ),
                      );
                    },
                  ),
          ),
        ],
      ),
    );
  }
}

/// A timestamp a person can read at a glance.
///
/// The host sends RFC3339; showing that raw made the list a wall of
/// `2026-07-27T03:57:29Z`, which is precise and unreadable.
String _when(String isoTimestamp) {
  final at = DateTime.tryParse(isoTimestamp)?.toLocal();
  if (at == null) return isoTimestamp;
  final gap = DateTime.now().difference(at);
  if (gap.inMinutes < 1) return '刚刚';
  if (gap.inMinutes < 60) return '${gap.inMinutes} 分钟前';
  if (gap.inHours < 24) return '${gap.inHours} 小时前';
  if (gap.inDays < 7) return '${gap.inDays} 天前';
  return '${at.year}-${at.month.toString().padLeft(2, '0')}-'
      '${at.day.toString().padLeft(2, '0')}';
}

class _GoalDialog extends StatefulWidget {
  const _GoalDialog();

  @override
  State<_GoalDialog> createState() => _GoalDialogState();
}

class _GoalDialogState extends State<_GoalDialog> {
  final TextEditingController _goal = TextEditingController();

  @override
  void dispose() {
    _goal.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('新建任务'),
      content: TextField(
        controller: _goal,
        autofocus: true,
        minLines: 2,
        maxLines: 4,
        decoration: const InputDecoration(
          labelText: '这次要做什么',
          border: OutlineInputBorder(),
        ),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.pop(context), child: const Text('取消')),
        FilledButton(
          onPressed: () => Navigator.pop(context, _goal.text.trim()),
          child: const Text('开始'),
        ),
      ],
    );
  }
}
