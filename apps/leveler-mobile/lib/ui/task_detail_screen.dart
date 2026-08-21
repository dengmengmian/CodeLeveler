/// Task workspace: a projection of the open session, not a second store.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import '../domain/artifact.dart';
import '../domain/session_state.dart';
import '../domain/task_status.dart';
import 'artifact_card.dart';
import 'artifact_preview.dart';
import 'status_chip.dart';
import 'timeline.dart';

class TaskDetailScreen extends StatelessWidget {
  const TaskDetailScreen({super.key, required this.controller});
  final AppController controller;

  @override
  Widget build(BuildContext context) {
    final session = controller.session;
    if (session == null) return const SizedBox.shrink();

    return ListenableBuilder(
      listenable: session,
      builder: (context, _) {
        final theme = Theme.of(context);
        return Scaffold(
          appBar: AppBar(
            title: const Text('任务'),
          ),
          body: ListView(
            padding: const EdgeInsets.fromLTRB(16, 12, 16, 32),
            children: [
              Text(
                session.goal.isEmpty ? '任务' : session.goal,
                style: theme.textTheme.titleLarge,
              ),
              const SizedBox(height: 10),
              Row(
                children: [
                  StatusChip(status: session.taskStatus),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Text(
                      session.activity ?? session.taskStatus.label,
                      maxLines: 2,
                      overflow: TextOverflow.ellipsis,
                      style: theme.textTheme.bodySmall,
                    ),
                  ),
                ],
              ),
              if (session.planSteps > 0) ...[
                const SizedBox(height: 20),
                Text('计划', style: theme.textTheme.titleSmall),
                const SizedBox(height: 6),
                Text(
                  '${session.planDone} / ${session.planSteps}',
                  style: theme.textTheme.bodyMedium,
                ),
                for (final title in session.planTitles)
                  Padding(
                    padding: const EdgeInsets.only(top: 6),
                    child: Text('· $title', style: theme.textTheme.bodySmall),
                  ),
              ],
              if (session.artifacts.isNotEmpty) ...[
                const SizedBox(height: 20),
                Text('产物', style: theme.textTheme.titleSmall),
                for (final artifact in session.artifacts)
                  ArtifactCard(
                    artifact: artifact,
                    onOpen: () => _open(context, artifact),
                  ),
              ],
              if (session.approvals.isNotEmpty) ...[
                const SizedBox(height: 20),
                Text('待审批', style: theme.textTheme.titleSmall),
                for (final approval in session.approvals.values)
                  Padding(
                    padding: const EdgeInsets.only(top: 8),
                    child: Text(approval.ask, style: theme.textTheme.bodyMedium),
                  ),
              ],
              if (session.timeline.isNotEmpty) ...[
                const SizedBox(height: 20),
                Text('时间线', style: theme.textTheme.titleSmall),
                const SizedBox(height: 8),
                for (final item in session.timeline)
                  TimelineRow(
                    item: item,
                    artifact: item.kind == TimelineKind.attachment
                        ? _artifact(session, item.id)
                        : null,
                    onOpenArtifact: item.kind == TimelineKind.attachment
                        ? () {
                            final artifact = _artifact(session, item.id);
                            if (artifact != null) _open(context, artifact);
                          }
                        : null,
                  ),
              ],
            ],
          ),
        );
      },
    );
  }

  void _open(BuildContext context, Artifact artifact) {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => ArtifactPreviewPage(
          artifact: artifact,
          onFetch: () => controller.fetchAttachment(artifact),
        ),
      ),
    );
  }
}

Artifact? _artifact(SessionState session, String id) {
  for (final artifact in session.artifacts) {
    if (artifact.id == id) return artifact;
  }
  return null;
}
