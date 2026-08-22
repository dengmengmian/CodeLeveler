/// leveler-mobile: remote control for a CodeLeveler machine you own.
///
/// The app is a client, not a participant: no model runs here, no repository
/// lives here, and nothing it displays is believed until the host's signature
/// on it has been checked.
library;

import 'package:flutter/material.dart';

import 'crypto/store.dart';
import 'domain/app_controller.dart';
import 'ui/chat_screen.dart';
import 'ui/home_screen.dart';
import 'ui/theme.dart';
import 'ui/pairing_screen.dart';
import 'ui/sessions_screen.dart';

void main() {
  runApp(LevelerApp(controller: AppController(vault: Vault(KeystoreSecretStore()))));
}

class LevelerApp extends StatefulWidget {
  const LevelerApp({super.key, required this.controller});
  final AppController controller;

  @override
  State<LevelerApp> createState() => _LevelerAppState();
}

class _LevelerAppState extends State<LevelerApp> {
  late final Future<void> _restored = widget.controller.restore();

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'CodeLeveler',
      theme: levelerTheme(Brightness.light),
      darkTheme: levelerTheme(Brightness.dark),
      home: FutureBuilder<void>(
        future: _restored,
        builder: (context, snapshot) {
          if (snapshot.connectionState != ConnectionState.done) {
            return const Scaffold(body: Center(child: CircularProgressIndicator()));
          }
          return _Root(controller: widget.controller);
        },
      ),
    );
  }
}

class _Root extends StatelessWidget {
  const _Root({required this.controller});
  final AppController controller;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: controller,
      builder: (context, _) {
        if (!controller.isPaired) {
          return PairingScreen(controller: controller);
        }
        if (controller.session != null) {
          return ChatScreen(controller: controller);
        }
        final projectId = controller.currentProjectId;
        if (projectId != null) {
          return SessionsScreen(
            controller: controller,
            projectName: controller.projects
                    .where((project) => project.id == projectId)
                    .map((project) => project.display)
                    .firstOrNull ??
                '项目',
          );
        }
        return HomeScreen(controller: controller);
      },
    );
  }
}

extension _FirstOrNull<T> on Iterable<T> {
  T? get firstOrNull => isEmpty ? null : first;
}
