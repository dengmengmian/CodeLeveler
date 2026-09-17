/// Talking to the relay, which is assumed hostile.
///
/// Everything here is *routing*: pairing, tokens, and posting an already-signed
/// envelope. No result from this file is trusted on its own — an RPC answer is
/// returned as an envelope for the caller to verify against the anchored
/// runtime key, and a caller that skipped that would be rendering whatever the
/// relay chose to send.
library;

import 'dart:convert';

import 'package:cryptography/cryptography.dart';
import 'package:http/http.dart' as http;

import '../crypto/keys.dart';
import '../protocol/envelope.dart';

/// A refusal from the relay, with the code its catalogue defines.
class RelayException implements Exception {
  RelayException(this.status, this.code, this.message);
  final int status;
  final String code;
  final String message;

  /// Whether retrying the same request could plausibly work.
  bool get isTransient => status == 503;

  /// Whether the phone must authenticate again.
  bool get needsReauth => status == 401;

  @override
  String toString() => 'RelayException($status/$code: $message)';
}

/// Tokens, and how long they last.
class SessionTokens {
  SessionTokens({
    required this.accessToken,
    required this.refreshToken,
    required this.expiresAt,
    required this.pairingScope,
  });

  final String accessToken;
  final String refreshToken;
  final DateTime expiresAt;

  /// `interactive` or `observe`. An observe pairing gets the same signed event
  /// stream and is refused every delivery, so the UI hides the composer rather
  /// than letting a user type into something that will always fail.
  final String pairingScope;

  bool get isObserveOnly => pairingScope == 'observe';

  /// Renew a little early: a token that expires mid-request costs a round trip
  /// and a visible error.
  bool needsRefresh(DateTime now) => now.isAfter(expiresAt.subtract(const Duration(minutes: 2)));
}

class RelayClient {
  RelayClient({required this.baseUrl, http.Client? httpClient})
      : _http = httpClient ?? http.Client();

  final String baseUrl;
  final http.Client _http;

  Uri _url(String path) => Uri.parse('$baseUrl$path');

  Never _fail(http.Response response) {
    String code = 'unknown';
    String message = response.body;
    try {
      final body = jsonDecode(response.body);
      if (body is Map<String, dynamic>) {
        code = body['code'] as String? ?? code;
        message = body['message'] as String? ?? message;
      }
    } on FormatException {
      // A body that is not JSON is still a failure; keep it as the message.
    }
    throw RelayException(response.statusCode, code, message);
  }

  /// Claim a pairing secret. The only unauthenticated call the app makes —
  /// which is the point of the secret.
  Future<void> completePairing({
    required String deviceId,
    required String devicePubkeyB64Url,
    required String deviceName,
    required String platform,
    required String pairingSecret,
    required String scope,
  }) async {
    final response = await _http.post(
      _url('/v1/pair/complete'),
      headers: const {'content-type': 'application/json'},
      body: jsonEncode({
        'device_id': deviceId,
        'device_pubkey': devicePubkeyB64Url,
        'device_name': deviceName,
        'platform': platform,
        'pairing_secret': pairingSecret,
        'scope': scope,
      }),
    );
    if (response.statusCode >= 400) _fail(response);
  }

  /// Prove possession of the paired key and get routing tokens.
  Future<SessionTokens> authenticate({
    required DeviceIdentity identity,
    required String runtimeId,
    required DateTime now,
    required String nonce,
  }) async {
    final timestamp = formatTimestamp(now);
    final assertion = '${identity.deviceId}|$runtimeId|$timestamp|$nonce';
    final signature = await Ed25519().sign(
      utf8.encode(assertion),
      keyPair: identity.keyPair,
    );
    final response = await _http.post(
      _url('/v1/auth/session'),
      headers: const {'content-type': 'application/json'},
      body: jsonEncode({
        'device_id': identity.deviceId,
        'runtime_id': runtimeId,
        'timestamp': timestamp,
        'nonce': nonce,
        'sig': base64.encode(signature.bytes),
      }),
    );
    if (response.statusCode >= 400) _fail(response);
    final body = jsonDecode(response.body) as Map<String, dynamic>;
    return SessionTokens(
      accessToken: body['access_token'] as String,
      refreshToken: body['refresh_token'] as String,
      expiresAt: now.add(Duration(seconds: (body['expires_in_secs'] as num).toInt())),
      pairingScope: body['pairing_scope'] as String? ?? 'interactive',
    );
  }

  /// Rotate the refresh token, proving the key is still held.
  ///
  /// A refresh token alone is a bearer credential; the assertion is what makes
  /// a stolen one useless without the phone.
  Future<SessionTokens> refresh({
    required DeviceIdentity identity,
    required String refreshToken,
    required DateTime now,
    required String pairingScope,
  }) async {
    final timestamp = formatTimestamp(now);
    final assertion = '${identity.deviceId}|$timestamp';
    final signature = await Ed25519().sign(
      utf8.encode(assertion),
      keyPair: identity.keyPair,
    );
    final response = await _http.post(
      _url('/v1/auth/refresh'),
      headers: const {'content-type': 'application/json'},
      body: jsonEncode({
        'refresh_token': refreshToken,
        'device_assertion': {
          'device_id': identity.deviceId,
          'timestamp': timestamp,
          'sig': base64.encode(signature.bytes),
        },
      }),
    );
    if (response.statusCode >= 400) _fail(response);
    final body = jsonDecode(response.body) as Map<String, dynamic>;
    return SessionTokens(
      accessToken: body['access_token'] as String,
      refreshToken: body['refresh_token'] as String,
      expiresAt: now.add(Duration(seconds: (body['expires_in_secs'] as num).toInt())),
      pairingScope: pairingScope,
    );
  }

  /// The machines this device is paired with, as the *relay* names them.
  ///
  /// Cosmetic only, and deliberately so: the name is unsigned, so a relay could
  /// call a host anything it liked. What identifies a machine is the key the
  /// pairing anchored, which is why the fingerprint — not this — is what the
  /// settings screen shows and what a user is asked to compare.
  Future<Map<String, String>> hosts({required String accessToken}) async {
    final response = await _http.get(
      _url('/v1/hosts'),
      headers: {'authorization': 'Bearer $accessToken'},
    );
    if (response.statusCode >= 400) _fail(response);
    final listed = jsonDecode(response.body) as List<dynamic>;
    return {
      for (final raw in listed.cast<Map<String, dynamic>>())
        (raw['host_id'] as String? ?? ''): (raw['display_name'] as String? ?? ''),
    };
  }

  /// Post one device-signed RPC and return the runtime's answer, still sealed.
  ///
  /// Returning the envelope rather than its contents is deliberate: the caller
  /// has to verify it, and a signature nobody checks is decoration.
  Future<SignedEnvelope> rpc({
    required String accessToken,
    required String hostId,
    required SignedEnvelope request,
  }) async {
    final response = await _http.post(
      _url('/v1/hosts/$hostId/rpc'),
      headers: {
        'content-type': 'application/json',
        'authorization': 'Bearer $accessToken',
      },
      body: jsonEncode(request.toJson()),
    );
    if (response.statusCode >= 400) _fail(response);
    return SignedEnvelope.fromJson(jsonDecode(response.body) as Map<String, dynamic>);
  }

  void close() => _http.close();
}
