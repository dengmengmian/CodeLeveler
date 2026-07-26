/// Where the phone keeps what must not leak.
///
/// The private key seed, the anchored runtime key, and the refresh token all go
/// to the platform keystore — Keychain on iOS, EncryptedSharedPreferences on
/// Android — and nowhere else. Two consequences worth stating: the key is
/// excluded from iCloud backup on iOS (`first_unlock_this_device`), and
/// "clear pairing" really deletes it, so a user who suspects a problem has a
/// way to make this installation forget everything.
///
/// The interface is small on purpose: a test double implements it in a dozen
/// lines, so protocol logic can be tested without a device.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

abstract class SecretStore {
  Future<String?> read(String key);
  Future<void> write(String key, String value);
  Future<void> delete(String key);
}

/// Platform-backed storage.
class KeystoreSecretStore implements SecretStore {
  KeystoreSecretStore()
      : _storage = const FlutterSecureStorage(
          iOptions: IOSOptions(
            // Not synchronised to iCloud, and unavailable until the device has
            // been unlocked once since boot.
            accessibility: KeychainAccessibility.first_unlock_this_device,
            synchronizable: false,
          ),
          aOptions: AndroidOptions(encryptedSharedPreferences: true),
        );

  final FlutterSecureStorage _storage;

  @override
  Future<String?> read(String key) => _storage.read(key: key);

  @override
  Future<void> write(String key, String value) => _storage.write(key: key, value: value);

  @override
  Future<void> delete(String key) => _storage.delete(key: key);
}

/// In-memory storage, for tests.
class MemorySecretStore implements SecretStore {
  final Map<String, String> _values = {};

  @override
  Future<String?> read(String key) async => _values[key];

  @override
  Future<void> write(String key, String value) async => _values[key] = value;

  @override
  Future<void> delete(String key) async => _values.remove(key);
}

/// What one paired host means to this phone.
class Pairing {
  Pairing({
    required this.relayUrl,
    required this.runtimeId,
    required this.runtimePubkeyB64Url,
    required this.deviceId,
  });

  final String relayUrl;
  final String runtimeId;

  /// Anchored at pairing. Every `sender=runtime` frame is verified against this
  /// and never against anything a relay sends later.
  final String runtimePubkeyB64Url;
  final String deviceId;

  Map<String, dynamic> toJson() => {
        'relay_url': relayUrl,
        'runtime_id': runtimeId,
        'runtime_pubkey': runtimePubkeyB64Url,
        'device_id': deviceId,
      };

  static Pairing fromJson(Map<String, dynamic> json) => Pairing(
        relayUrl: json['relay_url'] as String,
        runtimeId: json['runtime_id'] as String,
        runtimePubkeyB64Url: json['runtime_pubkey'] as String,
        deviceId: json['device_id'] as String,
      );
}

/// The keys under which secrets live, and the operations over them.
class Vault {
  Vault(this._store);
  final SecretStore _store;

  static const _seedKey = 'device_seed_b64';
  static const _pairingKey = 'pairing_json';
  static const _refreshKey = 'refresh_token';

  Future<Uint8List?> deviceSeed() async {
    final stored = await _store.read(_seedKey);
    if (stored == null) return null;
    return Uint8List.fromList(base64.decode(stored));
  }

  Future<void> saveDeviceSeed(Uint8List seed) =>
      _store.write(_seedKey, base64.encode(seed));

  Future<Pairing?> pairing() async {
    final stored = await _store.read(_pairingKey);
    if (stored == null) return null;
    return Pairing.fromJson(jsonDecode(stored) as Map<String, dynamic>);
  }

  Future<void> savePairing(Pairing pairing) =>
      _store.write(_pairingKey, jsonEncode(pairing.toJson()));

  Future<String?> refreshToken() => _store.read(_refreshKey);

  Future<void> saveRefreshToken(String token) => _store.write(_refreshKey, token);

  /// Forget this installation entirely.
  ///
  /// Deliberately includes the device key: leaving it behind would let a
  /// "cleared" phone be re-paired silently as the same identity the host may
  /// still have a record of.
  Future<void> clear() async {
    await _store.delete(_refreshKey);
    await _store.delete(_pairingKey);
    await _store.delete(_seedKey);
  }
}
