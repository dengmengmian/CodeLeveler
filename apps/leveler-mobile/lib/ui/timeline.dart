/// Agent timeline rows. Not chat bubbles.
library;

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import '../domain/artifact.dart';
import '../domain/session_state.dart';
import 'artifact_card.dart';

class TimelineRow extends StatelessWidget {
  const TimelineRow({
    super.key,
    required this.item,
    this.artifact,
    this.onOpenArtifact,
  });

  final TimelineItem item;
  final Artifact? artifact;
  final VoidCallback? onOpenArtifact;

  @override
  Widget build(BuildContext context) {
    return switch (item.kind) {
      TimelineKind.user => _UserRow(text: item.detail),
      TimelineKind.assistant => _AssistantRow(text: item.detail),
      TimelineKind.tool => _RailRow(
          icon: Icons.terminal,
          title: item.title,
          detail: item.detail,
        ),
      TimelineKind.toolResult => _RailRow(
          icon: item.ok == false ? Icons.error_outline : Icons.check_circle_outline,
          title: item.title,
          detail: item.detail,
          emphasize: item.ok == false,
        ),
      TimelineKind.plan => _RailRow(
          icon: Icons.account_tree_outlined,
          title: item.title,
          detail: item.detail,
        ),
      TimelineKind.attachment => artifact == null
          ? _RailRow(
              icon: Icons.attach_file,
              title: item.title,
              detail: item.detail.isEmpty ? '产物' : item.detail,
            )
          : ArtifactCard(artifact: artifact!, onOpen: onOpenArtifact),
      TimelineKind.approval => _RailRow(
          icon: Icons.gpp_maybe_outlined,
          title: item.title,
          detail: item.detail,
        ),
      TimelineKind.status => _StatusLine(text: item.title),
      TimelineKind.notice => _StatusLine(text: item.title),
      TimelineKind.thinking => _ThinkingRow(text: item.detail),
      TimelineKind.subAgent => _RailRow(
          icon: Icons.hub_outlined,
          title: item.title,
          detail: item.detail,
          emphasize: item.ok == false,
        ),
      TimelineKind.verification => _RailRow(
          icon: item.ok == false ? Icons.error_outline : Icons.verified_outlined,
          title: item.title,
          detail: item.detail,
          emphasize: item.ok == false,
        ),
      TimelineKind.diff => _RailRow(
          icon: Icons.difference_outlined,
          title: item.title,
          detail: item.detail,
        ),
    };
  }
}

class _UserRow extends StatelessWidget {
  const _UserRow({required this.text});
  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Container(
            width: 3,
            height: 18,
            margin: const EdgeInsets.only(top: 2, right: 10),
            color: theme.colorScheme.primary,
          ),
          Expanded(
            child: Text(text, style: theme.textTheme.bodyMedium),
          ),
        ],
      ),
    );
  }
}

class _AssistantRow extends StatelessWidget {
  const _AssistantRow({required this.text});
  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    if (text.isEmpty) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: MarkdownBody(
        data: text,
        selectable: false,
        styleSheet: MarkdownStyleSheet.fromTheme(theme).copyWith(
          p: theme.textTheme.bodyMedium,
          code: theme.textTheme.bodySmall?.copyWith(
            fontFamily: 'monospace',
            backgroundColor: theme.colorScheme.surfaceContainerHighest,
          ),
          codeblockDecoration: BoxDecoration(
            color: theme.colorScheme.surfaceContainerHighest,
            borderRadius: BorderRadius.circular(8),
          ),
        ),
      ),
    );
  }
}

class _RailRow extends StatelessWidget {
  const _RailRow({
    required this.icon,
    required this.title,
    required this.detail,
    this.emphasize = false,
  });

  final IconData icon;
  final String title;
  final String detail;
  final bool emphasize;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final color = emphasize ? theme.colorScheme.error : theme.colorScheme.onSurfaceVariant;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(icon, size: 16, color: color),
          const SizedBox(width: 8),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  title,
                  style: theme.textTheme.labelLarge?.copyWith(
                    fontFamily: 'monospace',
                    color: color,
                  ),
                ),
                if (detail.isNotEmpty)
                  Padding(
                    padding: const EdgeInsets.only(top: 2),
                    child: Text(
                      detail,
                      maxLines: 3,
                      overflow: TextOverflow.ellipsis,
                      style: theme.textTheme.bodySmall?.copyWith(
                        fontFamily: 'monospace',
                        color: theme.colorScheme.outline,
                      ),
                    ),
                  ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _ThinkingRow extends StatefulWidget {
  const _ThinkingRow({required this.text});
  final String text;

  @override
  State<_ThinkingRow> createState() => _ThinkingRowState();
}

class _ThinkingRowState extends State<_ThinkingRow> {
  bool _open = false;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: InkWell(
        onTap: () => setState(() => _open = !_open),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(Icons.psychology_outlined, size: 16, color: theme.colorScheme.outline),
            const SizedBox(width: 8),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    '思考中',
                    style: theme.textTheme.labelLarge?.copyWith(
                      color: theme.colorScheme.outline,
                    ),
                  ),
                  if (_open && widget.text.isNotEmpty)
                    Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: Text(
                        widget.text,
                        style: theme.textTheme.bodySmall?.copyWith(
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _StatusLine extends StatelessWidget {
  const _StatusLine({required this.text});
  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 10),
      child: Row(
        children: [
          Expanded(child: Divider(color: theme.colorScheme.outlineVariant)),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 10),
            child: Text(
              text,
              style: theme.textTheme.labelSmall?.copyWith(color: theme.colorScheme.outline),
            ),
          ),
          Expanded(child: Divider(color: theme.colorScheme.outlineVariant)),
        ],
      ),
    );
  }
}
