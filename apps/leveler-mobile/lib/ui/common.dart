/// Widgets more than one screen needs.
///
/// They live here rather than in `main.dart` so the screens do not have to
/// import the file that imports them — a cycle Dart tolerates and readers do
/// not.
library;

import 'package:flutter/material.dart';

import '../domain/app_controller.dart';
import 'settings_screen.dart';

/// A banner every screen can show, so connection trouble is stated once and in
/// one voice rather than as a different surprise per screen.
class StatusBanner extends StatelessWidget {
  const StatusBanner({super.key, required this.controller});
  final AppController controller;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final (String? text, Color color) = switch (controller.connection) {
      // The one state that is not a network problem: something claimed to be
      // the host and could not prove it.
      LinkState.untrusted => (
          controller.lastError ?? '收到无法验证的数据',
          theme.colorScheme.error,
        ),
      LinkState.offline => ('开发机离线，稍后重试', theme.colorScheme.tertiary),
      LinkState.connecting => ('连接中…', theme.colorScheme.secondary),
      _ => (controller.lastError, theme.colorScheme.error),
    };
    if (text == null) return const SizedBox.shrink();

    return Material(
      color: color.withValues(alpha: 0.12),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
        child: Row(
          children: [
            Icon(Icons.info_outline, size: 18, color: color),
            const SizedBox(width: 8),
            Expanded(child: Text(text, style: theme.textTheme.bodySmall)),
          ],
        ),
      ),
    );
  }
}

/// Shared entry into settings, where a user can revoke this installation.
IconButton settingsButton(BuildContext context, AppController controller) => IconButton(
      icon: const Icon(Icons.settings_outlined),
      tooltip: '设置',
      onPressed: () => Navigator.of(context).push(
        MaterialPageRoute<void>(
          builder: (_) => SettingsScreen(controller: controller),
        ),
      ),
    );
