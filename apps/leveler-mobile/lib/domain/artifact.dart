/// A projection of a host `AttachmentRef`. Bytes stay in the media store.
///
/// There is no public `download_url`. Fetching, when the host grows a signed
/// RPC, is keyed by [sha256] on the same pairing channel as everything else.
library;

/// Product type for cards and preview. Derived from mime / filename.
enum ArtifactType { markdown, diff, image, archive, other }

extension ArtifactTypeLabel on ArtifactType {
  String get label => switch (this) {
        ArtifactType.markdown => 'Markdown',
        ArtifactType.diff => 'Diff',
        ArtifactType.image => '图片',
        ArtifactType.archive => '压缩包',
        ArtifactType.other => '文件',
      };
}

class Artifact {
  Artifact({
    required this.id,
    required this.sessionId,
    required this.name,
    required this.type,
    required this.mimeType,
    required this.sizeBytes,
    required this.sha256,
    this.hostKind = '',
    this.width,
    this.height,
  });

  final String id;
  final String sessionId;

  /// CURRENT: one session is one task.
  String get taskId => sessionId;
  final String name;
  final ArtifactType type;
  final String mimeType;
  final int sizeBytes;
  final String sha256;
  final String hostKind;
  final int? width;
  final int? height;

  String get sizeLabel => formatBytes(sizeBytes);

  static Artifact fromAttachment(String sessionId, Map<String, dynamic> json) {
    final name = json['name'] as String? ?? '文件';
    final mime = json['mime_type'] as String? ?? '';
    return Artifact(
      id: json['id'] as String? ?? '',
      sessionId: sessionId,
      name: name,
      type: classifyArtifact(name: name, mimeType: mime),
      mimeType: mime,
      sizeBytes: (json['size_bytes'] as num?)?.toInt() ?? 0,
      sha256: json['sha256'] as String? ?? '',
      hostKind: json['kind'] as String? ?? '',
      width: (json['width'] as num?)?.toInt(),
      height: (json['height'] as num?)?.toInt(),
    );
  }
}

ArtifactType classifyArtifact({required String name, required String mimeType}) {
  final lower = name.toLowerCase();
  final mime = mimeType.toLowerCase();
  if (mime.startsWith('image/') ||
      lower.endsWith('.png') ||
      lower.endsWith('.jpg') ||
      lower.endsWith('.jpeg') ||
      lower.endsWith('.gif') ||
      lower.endsWith('.webp')) {
    return ArtifactType.image;
  }
  if (mime.contains('markdown') || lower.endsWith('.md')) {
    return ArtifactType.markdown;
  }
  if (mime.contains('diff') || lower.endsWith('.diff') || lower.endsWith('.patch')) {
    return ArtifactType.diff;
  }
  if (mime.contains('zip') ||
      mime.contains('tar') ||
      mime.contains('gzip') ||
      lower.endsWith('.zip') ||
      lower.endsWith('.tar') ||
      lower.endsWith('.tgz') ||
      lower.endsWith('.gz')) {
    return ArtifactType.archive;
  }
  return ArtifactType.other;
}

String formatBytes(int bytes) {
  if (bytes >= 1024 * 1024) {
    return '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MB';
  }
  if (bytes >= 1024) return '${bytes ~/ 1024} KB';
  return '$bytes B';
}
