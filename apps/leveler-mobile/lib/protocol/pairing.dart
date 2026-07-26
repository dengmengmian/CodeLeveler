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
  static PairingQrPayload parse(String text) {
    final Object? decoded;
    try {
      decoded = jsonDecode(text.trim());
    } on FormatException {
      throw const FormatException('这不是配对载荷：应当是一行 JSON');
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
