/// The sessions inside one project.
///
/// The list arrives as a runtime *event* on the project's stream rather than as
/// a reply to a request, so this screen shows "loading" until one turns up and
/// says plainly when none does — a blank list and a broken stream look the same
/// otherwise.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import 'common.dart';

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
              label: const Text('新会话'),
            ),
      body: Column(
        children: [
          StatusBanner(controller: controller),
          if (controller.sessionsLoading) const LinearProgressIndicator(),
          Expanded(
            child: controller.sessions.isEmpty
                ? Center(
                    child: Text(
                      controller.sessionsLoading ? '正在读取会话…' : '这个项目还没有会话',
                    ),
                  )
                : ListView.separated(
                    itemCount: controller.sessions.length,
                    separatorBuilder: (_, __) => const Divider(height: 1),
                    itemBuilder: (context, index) {
                      final session = controller.sessions[index];
                      return ListTile(
                        title: Text(
                          session.goal.isEmpty ? '(无目标)' : session.goal,
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis,
                        ),
                        subtitle: Text('${session.status} · ${session.updatedAt}'),
                        trailing: const Icon(Icons.chevron_right),
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
      title: const Text('新建会话'),
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
