/// Pairing: what the phone reads from the host, and what it sends back.
///
/// The payload the host prints carries `runtime_pubkey`. That is the anchor for
/// everything afterwards — the app stores it at pairing and verifies every
/// later `sender=runtime` frame against it, never against a key the relay
/// offers. A relay that could hand the app a key of its own could author every
/// snapshot and approval prompt the user sees.
library;

import 'dart:convert';

import 'ids.dart';

class PairingQrPayload {
  PairingQrPayload({
    required this.runtimeId,
    required this.runtimePubkey,
    required this.relayUrl,
    required this.pairingSecret,
  });

  final String runtimeId;

  /// Raw Ed25519 public key, base64url without padding.
  final String runtimePubkey;
  final String relayUrl;
  final String pairingSecret;

  /// Parse a scanned or pasted payload.
  ///
  /// Every field is required and the id is checked here, at the moment a user
  /// is about to trust a machine — not later, when a frame fails to verify for
  /// reasons nobody can explain.
  /// The prefix of the short form a QR code carries.
  static const String compactPrefix = 'LV1';

  /// Read either form.
  ///
  /// Two producers write these: the terminal's QR writes the short form,
  /// because JSON's field names cost about a hundred characters and every one
  /// of them is more modules in a code someone has to photograph off a screen;
  /// `leveler remote pair` prints the JSON. One reader takes both rather than
  /// leaving a user to know which is which.
  static PairingQrPayload parse(String text) {
    final trimmed = text.trim();
    if (trimmed.startsWith('$compactPrefix|')) return _parseCompact(trimmed);

    final Object? decoded;
    try {
      decoded = jsonDecode(trimmed);
    } on FormatException {
      throw const FormatException('这不是配对载荷：应当是二维码里那一行，或一行 JSON');
    }
    if (decoded is! Map<String, dynamic>) {
      throw const FormatException('配对载荷格式不对');
    }
    final runtimeId = decoded['runtime_id'] as String?;
    final runtimePubkey = decoded['runtime_pubkey'] as String?;
    final relayUrl = decoded['relay_url'] as String?;
    final secret = decoded['pairing_secret'] as String?;
    if (runtimeId == null || runtimePubkey == null || relayUrl == null || secret == null) {
      throw const FormatException('配对载荷缺字段（需要 runtime_id / runtime_pubkey / relay_url / pairing_secret）');
    }
    if (!isValidId(runtimeId)) {
      throw const FormatException('机器 id 不合法');
    }
    if (!relayUrl.startsWith('http://') && !relayUrl.startsWith('https://')) {
      throw const FormatException('relay 地址必须是 http(s)://');
    }
    return PairingQrPayload(
      runtimeId: runtimeId,
      runtimePubkey: runtimePubkey,
      relayUrl: relayUrl.replaceAll(RegExp(r'/+$'), ''),
      pairingSecret: secret,
    );
  }

  /// `LV1|<runtime_id>|<runtime_pubkey>|<relay_url>|<pairing_secret>`.
  ///
  /// `|` is safe as a separator for the same reason it separates the fields of
  /// a signing input: ids may not contain it, and neither base64url nor a URL
  /// produces one.
  static PairingQrPayload _parseCompact(String text) {
    final parts = text.split('|');
    if (parts.length != 5) {
      throw const FormatException('二维码内容不完整——可能只扫到了一半，再扫一次');
    }
    final runtimeId = parts[1];
    final runtimePubkey = parts[2];
    final relayUrl = parts[3];
    final secret = parts[4];
    if (runtimePubkey.isEmpty || relayUrl.isEmpty || secret.isEmpty) {
      throw const FormatException('二维码内容不完整——可能只扫到了一半，再扫一次');
    }
    if (!isValidId(runtimeId)) {
      throw const FormatException('机器 id 不合法');
    }
    if (!relayUrl.startsWith('http://') && !relayUrl.startsWith('https://')) {
      throw const FormatException('relay 地址必须是 http(s)://');
    }
    return PairingQrPayload(
      runtimeId: runtimeId,
      runtimePubkey: runtimePubkey,
      relayUrl: relayUrl.replaceAll(RegExp(r'/+$'), ''),
      pairingSecret: secret,
    );
  }
}

/// What a phone must show its user while the host waits for confirmation.
class PairingConfirmation {
  PairingConfirmation({required this.deviceFingerprint, required this.hostFingerprint});

  /// The user compares this with what their terminal prints. It is derived
  /// from the key this app actually holds, so a match means the host is about
  /// to trust *this* device rather than one a relay substituted.
  final String deviceFingerprint;

  /// Derived from the anchored runtime key, so the user can also tell they are
  /// pairing with the machine they think they are.
  final String hostFingerprint;
}
