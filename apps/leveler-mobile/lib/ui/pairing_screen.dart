/// Pairing, which is the one screen where the user is doing security work.
///
/// So it is built to make the comparison unavoidable rather than skippable: the
/// fingerprint of this device's key is shown in the same grouped form the
/// terminal prints, next to the fingerprint of the machine being paired with,
/// and the wording says what is being confirmed — a key, not a name.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../domain/app_controller.dart';
import '../protocol/pairing.dart';

class PairingScreen extends StatefulWidget {
  const PairingScreen({super.key, required this.controller});
  final AppController controller;

  @override
  State<PairingScreen> createState() => _PairingScreenState();
}

class _PairingScreenState extends State<PairingScreen> {
  final TextEditingController _payload = TextEditingController();
  final TextEditingController _name = TextEditingController(text: '我的手机');
  PairingQrPayload? _parsed;
  PairingConfirmation? _confirmation;
  String? _error;
  bool _working = false;
  bool _observeOnly = false;

  @override
  void dispose() {
    _payload.dispose();
    _name.dispose();
    super.dispose();
  }

  Future<void> _read() async {
    setState(() {
      _error = null;
      _parsed = null;
      _confirmation = null;
    });
    try {
      final parsed = PairingQrPayload.parse(_payload.text);
      final confirmation = await widget.controller.previewPairing(parsed);
      setState(() {
        _parsed = parsed;
        _confirmation = confirmation;
      });
    } on FormatException catch (error) {
      setState(() => _error = error.message);
    }
  }

  Future<void> _pair() async {
    final parsed = _parsed;
    if (parsed == null) return;
    // Read from the context before any await: afterwards this State may no
    // longer be mounted, and the analyzer is right to complain about it.
    final platform =
        Theme.of(context).platform == TargetPlatform.iOS ? 'ios' : 'android';
    setState(() {
      _working = true;
      _error = null;
    });
    try {
      await widget.controller.completePairing(
        parsed,
        deviceName: _name.text.trim().isEmpty ? '手机' : _name.text.trim(),
        platform: platform,
        scope: _observeOnly ? 'observe' : 'interactive',
      );
    } catch (error) {
      setState(() => _error = '$error');
    } finally {
      if (mounted) setState(() => _working = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('配对开发机')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Text('在电脑上运行 `leveler remote pair`，把它打印的那一行粘贴到这里。',
              style: theme.textTheme.bodyMedium),
          const SizedBox(height: 4),
          Text('（扫码尚未内置；粘贴是等价的路径。）', style: theme.textTheme.bodySmall),
          const SizedBox(height: 16),
          TextField(
            controller: _payload,
            minLines: 3,
            maxLines: 6,
            decoration: InputDecoration(
              labelText: '配对载荷',
              border: const OutlineInputBorder(),
              suffixIcon: IconButton(
                icon: const Icon(Icons.content_paste),
                tooltip: '粘贴',
                onPressed: () async {
                  final data = await Clipboard.getData(Clipboard.kTextPlain);
                  if (data?.text != null) _payload.text = data!.text!;
                },
              ),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _name,
            decoration: const InputDecoration(
              labelText: '这台设备的名字（电脑上会显示）',
              border: OutlineInputBorder(),
            ),
          ),
          SwitchListTile(
            value: _observeOnly,
            onChanged: (value) => setState(() => _observeOnly = value),
            title: const Text('只读配对'),
            subtitle: const Text('可以看会话与事件，不能发送任何指令'),
          ),
          const SizedBox(height: 8),
          FilledButton(onPressed: _read, child: const Text('读取载荷')),
          if (_error != null) ...[
            const SizedBox(height: 16),
            Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
          ],
          if (_confirmation != null) ...[
            const SizedBox(height: 24),
            Card(
              child: Padding(
                padding: const EdgeInsets.all(16),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text('请在电脑上核对指纹', style: theme.textTheme.titleMedium),
                    const SizedBox(height: 12),
                    _Fingerprint(
                      label: '本机（这台手机）的指纹',
                      value: _confirmation!.deviceFingerprint,
                      hint: '电脑上 `leveler remote confirm` 显示的应当与这一行完全一致',
                    ),
                    const SizedBox(height: 12),
                    _Fingerprint(
                      label: '开发机的指纹',
                      value: _confirmation!.hostFingerprint,
                      hint: '与电脑上 `leveler remote status` 的公钥指纹一致',
                    ),
                    const SizedBox(height: 12),
                    Text(
                      '你确认的是这把密钥，不是设备名字。名字可以被中继改写，密钥不能。',
                      style: theme.textTheme.bodySmall,
                    ),
                  ],
                ),
              ),
            ),
            const SizedBox(height: 16),
            FilledButton.icon(
              onPressed: _working ? null : _pair,
              icon: _working
                  ? const SizedBox(
                      width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2))
                  : const Icon(Icons.link),
              label: const Text('指纹一致，提交配对'),
            ),
            const SizedBox(height: 8),
            Text('提交后，请在电脑上运行 `leveler remote confirm` 接受。',
                style: theme.textTheme.bodySmall),
          ],
        ],
      ),
    );
  }
}

class _Fingerprint extends StatelessWidget {
  const _Fingerprint({required this.label, required this.value, required this.hint});
  final String label;
  final String value;
  final String hint;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(label, style: theme.textTheme.labelLarge),
        const SizedBox(height: 4),
        SelectableText(
          value,
          style: theme.textTheme.titleLarge?.copyWith(
            fontFeatures: const [FontFeature.tabularFigures()],
            fontFamily: 'monospace',
            letterSpacing: 1.5,
          ),
        ),
        Text(hint, style: theme.textTheme.bodySmall),
      ],
    );
  }
}
