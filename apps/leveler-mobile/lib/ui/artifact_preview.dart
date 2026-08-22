/// Preview of one artifact. Bytes come from a signed `fetch_attachment` RPC,
/// never from a public URL or a workspace path.
library;

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import '../domain/artifact.dart';

enum _PreviewPhase { loading, success, error, empty }

class ArtifactPreviewPage extends StatefulWidget {
  const ArtifactPreviewPage({
    super.key,
    required this.artifact,
    this.bytes,
    this.onFetch,
  });

  final Artifact artifact;
  final List<int>? bytes;
  final Future<List<int>> Function()? onFetch;

  @override
  State<ArtifactPreviewPage> createState() => _ArtifactPreviewPageState();
}

class _ArtifactPreviewPageState extends State<ArtifactPreviewPage> {
  _PreviewPhase _phase = _PreviewPhase.empty;
  List<int>? _bytes;
  String? _error;

  @override
  void initState() {
    super.initState();
    final given = widget.bytes;
    if (given != null && given.isNotEmpty) {
      _bytes = given;
      _phase = _PreviewPhase.success;
    } else if (widget.onFetch != null) {
      _load();
    } else {
      _phase = _PreviewPhase.empty;
    }
  }

  Future<void> _load() async {
    setState(() {
      _phase = _PreviewPhase.loading;
      _error = null;
    });
    try {
      final data = await widget.onFetch!();
      if (!mounted) return;
      setState(() {
        _bytes = data;
        _phase = data.isEmpty ? _PreviewPhase.empty : _PreviewPhase.success;
      });
    } catch (error) {
      if (!mounted) return;
      setState(() {
        _error = error.toString();
        _phase = _PreviewPhase.error;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.artifact.name, maxLines: 1, overflow: TextOverflow.ellipsis),
        actions: [
          if (_phase == _PreviewPhase.success && _bytes != null)
            IconButton(
              tooltip: '复制内容',
              icon: const Icon(Icons.copy_outlined),
              onPressed: _copy,
            ),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.fromLTRB(16, 16, 16, 32),
        children: [
          Text(
            '${widget.artifact.type.label} · ${widget.artifact.sizeLabel}',
            style: theme.textTheme.labelMedium?.copyWith(
              color: theme.colorScheme.onSurfaceVariant,
            ),
          ),
          const SizedBox(height: 16),
          _body(theme),
        ],
      ),
    );
  }

  Widget _body(ThemeData theme) {
    return switch (_phase) {
      _PreviewPhase.loading => const Padding(
          padding: EdgeInsets.only(top: 48),
          child: Center(child: CircularProgressIndicator()),
        ),
      _PreviewPhase.error => Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              '产物加载失败',
              style: theme.textTheme.titleSmall?.copyWith(color: theme.colorScheme.error),
            ),
            const SizedBox(height: 8),
            Text(_error ?? '未知错误', style: theme.textTheme.bodyMedium),
            const SizedBox(height: 16),
            if (widget.onFetch != null)
              OutlinedButton(onPressed: _load, child: const Text('重试')),
          ],
        ),
      _PreviewPhase.empty => Text(
          '没有可预览的内容。产物必须先由 Runtime 登记为 attachment，这台手机不会去扫仓库。',
          style: theme.textTheme.bodyMedium,
        ),
      _PreviewPhase.success => _rendered(theme, _bytes!),
    };
  }

  Widget _rendered(ThemeData theme, List<int> data) {
    if (widget.artifact.type == ArtifactType.archive) {
      return Text(
        '压缩包不解压。拿到字节后可以复制元数据，而不是在手机里当文件管理器打开。',
        style: theme.textTheme.bodyMedium,
      );
    }
    if (widget.artifact.type == ArtifactType.image) {
      return Image.memory(Uint8List.fromList(data));
    }
    final text = utf8.decode(data, allowMalformed: true);
    if (widget.artifact.type == ArtifactType.markdown) {
      return MarkdownBody(data: text, selectable: true);
    }
    return SelectableText(
      text,
      style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
    );
  }

  Future<void> _copy() async {
    final data = _bytes;
    if (data == null) return;
    if (widget.artifact.type == ArtifactType.image) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('图片请在预览里查看。系统分享下一步再接。')),
      );
      return;
    }
    await Clipboard.setData(ClipboardData(text: utf8.decode(data, allowMalformed: true)));
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(const SnackBar(content: Text('已复制')));
  }
}
