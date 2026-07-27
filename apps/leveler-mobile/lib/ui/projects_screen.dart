/// The projects open on the paired machine.
///
/// Offline projects stay listed and greyed rather than disappearing: a project
/// vanishing looks like the user's mistake, where "the daemon on your computer
/// is not running" is something they can act on.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import 'common.dart';
import 'theme.dart';

class ProjectsScreen extends StatefulWidget {
  const ProjectsScreen({super.key, required this.controller});
  final AppController controller;

  @override
  State<ProjectsScreen> createState() => _ProjectsScreenState();
}

class _ProjectsScreenState extends State<ProjectsScreen> {
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) => widget.controller.loadProjects());
  }

  Future<void> _open(String projectId) => widget.controller.openProject(projectId);

  @override
  Widget build(BuildContext context) {
    final controller = widget.controller;
    return Scaffold(
      appBar: AppBar(
        // Two lines: what this screen lists, and which machine it is listing
        // from. A phone paired with one Mac never wonders; a phone paired with
        // two had no way to tell them apart.
        title: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Text('项目'),
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
              child: controller.projects.isEmpty
                  ? ListView(
                      children: [
                        const SizedBox(height: 100),
                        Icon(Icons.folder_off_outlined,
                            size: 40, color: Theme.of(context).colorScheme.outline),
                        const SizedBox(height: 14),
                        Center(
                          child: Text('电脑上没有已打开的项目',
                              style: Theme.of(context).textTheme.titleSmall),
                        ),
                        const SizedBox(height: 6),
                        Center(
                          child: Text(
                            '在电脑上打开一个仓库后，这里就会出现',
                            style: Theme.of(context).textTheme.bodySmall?.copyWith(
                                  color: Theme.of(context).colorScheme.onSurfaceVariant,
                                ),
                          ),
                        ),
                      ],
                    )
                  : ListView.separated(
                      itemCount: controller.projects.length,
                      separatorBuilder: (_, __) => const Divider(height: 1),
                      itemBuilder: (context, index) {
                        final project = controller.projects[index];
                        final theme = Theme.of(context);
                        return ListTile(
                          enabled: project.isOnline,
                          leading: StatusDot(online: project.isOnline),
                          title: Text(
                            project.display,
                            style: theme.textTheme.bodyLarge?.copyWith(
                              fontWeight: FontWeight.w500,
                            ),
                          ),
                          subtitle: Text(
                            project.isOnline ? '在线' : '离线 · 电脑上没有在跑',
                            style: theme.textTheme.bodySmall?.copyWith(
                              color: theme.colorScheme.onSurfaceVariant,
                            ),
                          ),
                          trailing: Icon(Icons.chevron_right,
                              size: 20, color: theme.colorScheme.outline),
                          onTap: project.isOnline ? () => _open(project.id) : null,
                        );
                      },
                    ),
            ),
          ),
        ],
      ),
    );
  }
}
