/// Cross-project view of what agents are doing. Not a chat inbox.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import '../domain/task_status.dart';
import 'common.dart';
import 'status_chip.dart';
import 'theme.dart';

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, required this.controller});
  final AppController controller;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) => widget.controller.loadProjects());
  }

  @override
  Widget build(BuildContext context) {
    final controller = widget.controller;
    return ListenableBuilder(
      listenable: controller,
      builder: (context, _) {
        final running = controller.runningTasks
            .where(
              (row) =>
                  row.$2.taskStatus == TaskStatus.running ||
                  row.$2.taskStatus == TaskStatus.planning,
            )
            .toList();
        final waiting = controller.waitingTasks;
        return Scaffold(
          appBar: AppBar(
            title: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Text('工作台'),
                if (controller.hostName.isNotEmpty)
                  Text(
                    controller.hostName,
                    style: Theme.of(context).textTheme.labelSmall?.copyWith(
                          color: Theme.of(context).colorScheme.onSurfaceVariant,
                        ),
                  ),
              ],
            ),
            actions: [settingsButton(context, controller)],
          ),
          body: Column(
            children: [
              StatusBanner(controller: controller),
              Expanded(
                child: RefreshIndicator(
                  onRefresh: controller.loadProjects,
                  child: ListView(
                    padding: const EdgeInsets.only(bottom: 24),
                    children: [
                      if (waiting.isNotEmpty) ...[
                        const _SectionTitle('需要你'),
                        for (final row in waiting)
                          _TaskTile(
                            project: row.$1.display,
                            session: row.$2,
                            onTap: () => controller.openSession(row.$1.id, row.$2.id),
                          ),
                      ],
                      if (running.isNotEmpty) ...[
                        const _SectionTitle('正在运行'),
                        for (final row in running)
                          _TaskTile(
                            project: row.$1.display,
                            session: row.$2,
                            onTap: () => controller.openSession(row.$1.id, row.$2.id),
                          ),
                      ],
                      const _SectionTitle('项目'),
                      if (controller.projects.isEmpty)
                        const _EmptyProjects()
                      else
                        for (final project in controller.projects)
                          ListTile(
                            enabled: project.isOnline,
                            leading: StatusDot(online: project.isOnline),
                            title: Text(
                              project.display,
                              style: Theme.of(context).textTheme.bodyLarge?.copyWith(
                                    fontWeight: FontWeight.w500,
                                  ),
                            ),
                            subtitle: Text(
                              project.isOnline ? '在线' : '离线 · 电脑上没有在跑',
                              style: Theme.of(context).textTheme.bodySmall?.copyWith(
                                    color: Theme.of(context).colorScheme.onSurfaceVariant,
                                  ),
                            ),
                            trailing: Icon(
                              Icons.chevron_right,
                              size: 20,
                              color: Theme.of(context).colorScheme.outline,
                            ),
                            onTap: project.isOnline ? () => controller.openProject(project.id) : null,
                          ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        );
      },
    );
  }
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text);
  final String text;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 18, 16, 6),
      child: Text(
        text,
        style: Theme.of(context).textTheme.titleSmall?.copyWith(
              color: Theme.of(context).colorScheme.onSurfaceVariant,
            ),
      ),
    );
  }
}

class _TaskTile extends StatelessWidget {
  const _TaskTile({required this.project, required this.session, required this.onTap});
  final String project;
  final SessionSummary session;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return ListTile(
      title: Text(
        session.goal.isEmpty ? '(无目标)' : session.goal,
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
      ),
      subtitle: Text(project, maxLines: 1, overflow: TextOverflow.ellipsis),
      trailing: StatusChip(status: session.taskStatus, compact: true),
      onTap: onTap,
    );
  }
}

class _EmptyProjects extends StatelessWidget {
  const _EmptyProjects();

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 48),
      child: Column(
        children: [
          Icon(Icons.folder_off_outlined, size: 40, color: theme.colorScheme.outline),
          const SizedBox(height: 14),
          Text('电脑上没有已打开的项目', style: theme.textTheme.titleSmall),
          const SizedBox(height: 6),
          Text(
            '在电脑上打开一个仓库后，这里就会出现',
            style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
          ),
        ],
      ),
    );
  }
}
