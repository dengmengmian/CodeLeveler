/// The projects open on the paired machine.
///
/// Offline projects stay listed and greyed rather than disappearing: a project
/// vanishing looks like the user's mistake, where "the daemon on your computer
/// is not running" is something they can act on.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import 'common.dart';

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
        title: const Text('项目'),
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
                      children: const [
                        SizedBox(height: 80),
                        Center(child: Text('电脑上没有已打开的项目')),
                        SizedBox(height: 8),
                        Center(child: Text('在电脑上用 `leveler web` 打开一个仓库')),
                      ],
                    )
                  : ListView.separated(
                      itemCount: controller.projects.length,
                      separatorBuilder: (_, __) => const Divider(height: 1),
                      itemBuilder: (context, index) {
                        final project = controller.projects[index];
                        return ListTile(
                          enabled: project.isOnline,
                          leading: Icon(
                            project.isOnline ? Icons.folder_open : Icons.folder_off_outlined,
                          ),
                          title: Text(project.display),
                          subtitle: Text(project.isOnline ? '在线' : '离线（电脑上未运行）'),
                          trailing: const Icon(Icons.chevron_right),
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
