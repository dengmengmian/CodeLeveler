/// Pairing, which is the one screen where the user is doing security work.
///
/// So it is built to make the comparison unavoidable rather than skippable: the
/// fingerprint of this device's key is shown in the same grouped form the
/// terminal prints, next to the fingerprint of the machine being paired with,
/// and the wording says what is being confirmed — a key, not a name.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

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

  /// Scan the code the terminal drew.
  ///
  /// The QR carries the same payload as the text field, so both paths converge
  /// on one parser — and on the same fingerprint comparison. A simulator has no
  /// camera, which is why pasting stays.
  Future<void> _scan() async {
    final scanned = await Navigator.of(context).push<String>(
      MaterialPageRoute(builder: (_) => const _ScannerScreen()),
    );
    if (scanned == null || !mounted) return;
    _payload.text = scanned;
    await _read();
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
        padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
        children: [
          // The instruction is the whole screen for a first-time user, so it
          // gets the weight of one rather than sitting as a paragraph above a
          // pile of fields.
          Container(
            padding: const EdgeInsets.all(16),
            decoration: BoxDecoration(
              color: theme.colorScheme.surfaceContainerHighest,
              borderRadius: BorderRadius.circular(16),
            ),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Icon(Icons.qr_code_2, size: 28, color: theme.colorScheme.primary),
                const SizedBox(width: 14),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text('在电脑上输入 /remote', style: theme.textTheme.titleSmall),
                      const SizedBox(height: 6),
                      Text(
                        '屏幕上会出现一个二维码，用下面的「扫码」扫它。',
                        style: theme.textTheme.bodySmall?.copyWith(
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                      ),
                      const SizedBox(height: 4),
                      Text(
                        '扫不了的时候，二维码下面那一行也可以直接粘贴过来。',
                        style: theme.textTheme.bodySmall?.copyWith(
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ],
                  ),
                ),
              ],
            ),
          ),
          const SizedBox(height: 20),
          // Scanning is the path; pasting is the fallback. The buttons now say
          // so by sitting above the field they are an alternative to.
          Row(
            children: [
              Expanded(
                child: FilledButton.icon(
                  onPressed: _scan,
                  icon: const Icon(Icons.qr_code_scanner),
                  label: const Text('扫码'),
                ),
              ),
              const SizedBox(width: 10),
              Expanded(
                child: OutlinedButton(onPressed: _read, child: const Text('读取粘贴的载荷')),
              ),
            ],
          ),
          const SizedBox(height: 20),
          if (_confirmation == null)
            TextField(
              controller: _payload,
              minLines: 3,
              maxLines: 6,
              decoration: InputDecoration(
                labelText: '配对载荷',
                alignLabelWithHint: true,
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
          if (_confirmation == null) const SizedBox(height: 12),
          TextField(
            controller: _name,
            decoration: const InputDecoration(
              labelText: '这台设备的名字（电脑上会显示）',
            ),
          ),
          const SizedBox(height: 4),
          SwitchListTile(
            value: _observeOnly,
            onChanged: (value) => setState(() => _observeOnly = value),
            title: const Text('只读配对'),
            subtitle: const Text('可以看会话与事件，不能发送任何指令'),
            contentPadding: EdgeInsets.zero,
          ),
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
            if (widget.controller.awaitingHostConfirmation)
              Row(
                children: [
                  const SizedBox(
                      width: 14, height: 14, child: CircularProgressIndicator(strokeWidth: 2)),
                  const SizedBox(width: 8),
                  Expanded(
                    child: Text('等待电脑确认——请在电脑上运行 `leveler remote confirm`',
                        style: theme.textTheme.bodySmall),
                  ),
                ],
              )
            else
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

/// A camera that returns the first payload it recognises.
class _ScannerScreen extends StatefulWidget {
  const _ScannerScreen();

  @override
  State<_ScannerScreen> createState() => _ScannerScreenState();
}

class _ScannerScreenState extends State<_ScannerScreen> {
  bool _handled = false;

  /// What went wrong with the camera, if anything did.
  ///
  /// The plugin's default handler for a detection error does nothing at all,
  /// and without an `errorBuilder` a camera that cannot start takes the screen
  /// with it. On a phone that means the app appears to die the moment the user
  /// taps 扫码 — with nothing said about a permission, a busy camera, or a
  /// device that has none.
  String? _failure;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('扫描电脑上的二维码')),
      body: _failure != null
          ? _ScannerProblem(message: _failure!)
          : MobileScanner(
              errorBuilder: (context, error) =>
                  _ScannerProblem(message: '打不开相机：${error.errorCode.name}'),
              onDetectError: (error, stack) {
                // Reading a frame failed. Say so rather than leave a live
                // preview that will never produce a result.
                if (mounted && _failure == null) {
                  setState(() => _failure = '识别出错：$error');
                }
              },
              onDetect: (capture) {
                // One result only: the camera fires repeatedly, and popping
                // twice would tear down the screen underneath as well.
                if (_handled || !mounted) return;
                for (final barcode in capture.barcodes) {
                  final value = barcode.rawValue;
                  if (value != null && value.isNotEmpty) {
                    _handled = true;
                    Navigator.of(context).pop(value);
                    return;
                  }
                }
              },
            ),
      bottomNavigationBar: SafeArea(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Text(
            '对准电脑上 /remote-loc 显示的那个码。扫不了就返回，用粘贴。',
            textAlign: TextAlign.center,
            style: theme.textTheme.bodySmall,
          ),
        ),
      ),
    );
  }
}

/// A camera that will not work, explained rather than crashed.
class _ScannerProblem extends StatelessWidget {
  const _ScannerProblem({required this.message});
  final String message;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(32),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(Icons.no_photography_outlined, size: 40, color: theme.colorScheme.outline),
            const SizedBox(height: 16),
            Text(message, textAlign: TextAlign.center, style: theme.textTheme.bodyMedium),
            const SizedBox(height: 20),
            OutlinedButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('返回，改用粘贴'),
            ),
          ],
        ),
      ),
    );
  }
}
