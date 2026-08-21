/// One generated file on the timeline. Not a file manager row.
library;

import 'package:flutter/material.dart';

import '../domain/artifact.dart';

class ArtifactCard extends StatelessWidget {
  const ArtifactCard({super.key, required this.artifact, this.onOpen});

  final Artifact artifact;
  final VoidCallback? onOpen;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Material(
        color: theme.colorScheme.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(10),
        child: InkWell(
          onTap: onOpen,
          borderRadius: BorderRadius.circular(10),
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
            child: Row(
              children: [
                Icon(_icon, size: 20, color: theme.colorScheme.primary),
                const SizedBox(width: 10),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        artifact.name,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: theme.textTheme.bodyMedium?.copyWith(
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                      const SizedBox(height: 2),
                      Text(
                        '${artifact.type.label} · ${artifact.sizeLabel}',
                        style: theme.textTheme.labelSmall?.copyWith(
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ],
                  ),
                ),
                Icon(Icons.chevron_right, size: 18, color: theme.colorScheme.outline),
              ],
            ),
          ),
        ),
      ),
    );
  }

  IconData get _icon => switch (artifact.type) {
        ArtifactType.markdown => Icons.description_outlined,
        ArtifactType.diff => Icons.code,
        ArtifactType.image => Icons.image_outlined,
        ArtifactType.archive => Icons.folder_zip_outlined,
        ArtifactType.other => Icons.insert_drive_file_outlined,
      };
}
