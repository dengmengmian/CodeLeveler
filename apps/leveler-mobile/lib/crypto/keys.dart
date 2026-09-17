/// This device's identity, and how a human checks it.
///
/// The private key never leaves the platform keystore, is never logged, and is
/// never sent anywhere — the pairing request carries only the public half. What
/// the user confirms on their terminal is the *fingerprint of that public key*,
/// so the two sides must derive it identically: the first eight bytes of
/// `SHA-256(raw public key)`, sixteen lowercase hex characters, grouped in
/// fours for reading aloud.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:cryptography/cryptography.dart';

/// An Ed25519 identity for this installation.
class DeviceIdentity {
  DeviceIdentity({required this.keyPair, required this.publicKey, required this.deviceId});

  final SimpleKeyPair keyPair;
  final SimplePublicKey publicKey;

  /// Derived from the public key, so it cannot be claimed by another device and
  /// needs no coordination to be unique.
  final String deviceId;

  static Future<DeviceIdentity> generate() async {
    final keyPair = await Ed25519().newKeyPair();
    return _from(keyPair);
  }

  static Future<DeviceIdentity> fromSeed(List<int> seed) async {
    final keyPair = await Ed25519().newKeyPairFromSeed(seed);
    return _from(keyPair);
  }

  static Future<DeviceIdentity> _from(SimpleKeyPair keyPair) async {
    final publicKey = await keyPair.extractPublicKey();
    final id = 'dev_${await fingerprintOf(publicKey.bytes)}';
    return DeviceIdentity(keyPair: keyPair, publicKey: publicKey, deviceId: id);
  }

  /// The 32-byte public key as base64url without padding — the form the host
  /// stores and the pairing request carries.
  String publicKeyB64Url() => base64Url.encode(publicKey.bytes).replaceAll('=', '');

  Future<String> fingerprint() => fingerprintOf(publicKey.bytes);

  Future<String> fingerprintDisplay() async => groupFingerprint(await fingerprint());

  /// The seed, for storing in the keystore. Callers must not put this anywhere
  /// else — not a log, not a backup, not analytics.
  Future<Uint8List> extractSeed() async =>
      Uint8List.fromList(await keyPair.extractPrivateKeyBytes());
}

/// `SHA-256(raw key)[0..8]` as sixteen lowercase hex characters.
Future<String> fingerprintOf(List<int> publicKeyBytes) async {
  final digest = await Sha256().hash(publicKeyBytes);
  return digest.bytes.take(8).map((b) => b.toRadixString(16).padLeft(2, '0')).join();
}

/// `abcd efgh ijkl mnop` — the form a person can read out and compare.
String groupFingerprint(String hex) {
  final groups = <String>[];
  for (var i = 0; i < hex.length; i += 4) {
    groups.add(hex.substring(i, i + 4 > hex.length ? hex.length : i + 4));
  }
  return groups.join(' ');
}

/// Decode a base64url public key from the pairing payload.
SimplePublicKey publicKeyFromB64Url(String encoded) {
  final padded = encoded.padRight(encoded.length + (4 - encoded.length % 4) % 4, '=');
  final bytes = base64Url.decode(padded);
  if (bytes.length != 32) {
    throw const FormatException('Ed25519 公钥必须是 32 字节');
  }
  return SimplePublicKey(bytes, type: KeyPairType.ed25519);
}
