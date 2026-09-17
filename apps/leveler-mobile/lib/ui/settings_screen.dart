/// What this installation is, and how to make it stop being it.
///
/// The unpair button deletes the device key as well as the pairing. Keeping the
/// key would let a "cleared" phone be re-paired as the same identity the host
/// may still have a record of — so clearing means clearing.
library;

import 'package:flutter/material.dart';

import '../crypto/keys.dart';
import '../domain/app_controller.dart';

class SettingsScreen extends StatelessWidget {
  const SettingsScreen({super.key, required this.controller});
  final AppController controller;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final pairing = controller.pairing;
    final unknown = controller.session?.unknownEvents ?? const <String, int>{};

    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: ListView(
        children: [
          if (pairing != null) ...[
            ListTile(
              title: const Text('开发机'),
              // The name if the relay gave one, with the id under it — the id
              // is what everything else is keyed by, so it stays visible.
              subtitle: Text(
                controller.hostName.isEmpty
                    ? pairing.runtimeId
                    : '${controller.hostName}\n${pairing.runtimeId}',
              ),
            ),
            ListTile(
              title: const Text('relay'),
              subtitle: Text(pairing.relayUrl),
            ),
            FutureBuilder<String>(
              future: controller.identity!.fingerprintDisplay(),
              builder: (context, snapshot) => ListTile(
                title: const Text('本机指纹'),
                subtitle: Text(snapshot.data ?? '…'),
              ),
            ),
            FutureBuilder<String>(
              future: fingerprintOf(
                publicKeyFromB64Url(pairing.runtimePubkeyB64Url).bytes,
              ).then(groupFingerprint),
              builder: (context, snapshot) => ListTile(
                title: const Text('开发机指纹（配对时锚定）'),
                subtitle: Text(snapshot.data ?? '…'),
              ),
            ),
            ListTile(
              title: const Text('配对权限'),
              subtitle: Text(controller.isObserveOnly ? '只读' : '可交互'),
            ),
          ],
          const Divider(),
          const ListTile(
            title: Text('协议版本'),
            subtitle: Text('leveler.session.v1 · 信封 v1'),
          ),
          if (unknown.isNotEmpty)
            ListTile(
              title: const Text('未识别的事件'),
              subtitle: Text(
                unknown.entries.map((e) => '${e.key} ×${e.value}').join('、'),
              ),
              trailing: const Icon(Icons.system_update_alt),
              // Stated rather than hidden: it means the host is newer than this
              // app, and the transcript may be missing something.
              onTap: () => showDialog<void>(
                context: context,
                builder: (context) => AlertDialog(
                  title: const Text('电脑端比这个 APP 新'),
                  content: const Text(
                    '有些事件这个版本读不懂，已经忽略并重新同步过。升级 APP 可以看到完整内容。',
                  ),
                  actions: [
                    TextButton(
                      onPressed: () => Navigator.pop(context),
                      child: const Text('知道了'),
                    ),
                  ],
                ),
              ),
            ),
          const Divider(),
          ListTile(
            leading: Icon(Icons.link_off, color: theme.colorScheme.error),
            title: Text('清除配对', style: TextStyle(color: theme.colorScheme.error)),
            subtitle: const Text('删除本机密钥与令牌。之后需要重新配对。'),
            onTap: () async {
              final confirmed = await showDialog<bool>(
                context: context,
                builder: (context) => AlertDialog(
                  title: const Text('清除配对？'),
                  content: const Text(
                    '这台手机的密钥会被删除，重新配对时会是一台新设备。\n'
                    '电脑上的记录不会自动消失——请另外运行 `leveler remote revoke <id>`。',
                  ),
                  actions: [
                    TextButton(
                      onPressed: () => Navigator.pop(context, false),
                      child: const Text('取消'),
                    ),
                    FilledButton(
                      onPressed: () => Navigator.pop(context, true),
                      child: const Text('清除'),
                    ),
                  ],
                ),
              );
              if (confirmed ?? false) {
                await controller.unpair();
                if (context.mounted) Navigator.of(context).pop();
              }
            },
          ),
        ],
      ),
    );
  }
}
