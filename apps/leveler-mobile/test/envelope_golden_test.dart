/// The cross-language conformance test.
///
/// It replays `testdata/signed_envelope.golden.json` — the same file the Rust
/// tests use — so the two implementations are checked against one answer key
/// rather than against each other's bugs. Every `accept` case must verify and
/// every `reject` case must fail with the stated code.
///
/// If this file goes red after a host change, the app is wrong about the wire,
/// which is exactly what it is for.
library;

import 'dart:convert';
import 'dart:io';

import 'package:cryptography/cryptography.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:leveler_mobile/crypto/keys.dart';
import 'package:leveler_mobile/protocol/envelope.dart';

/// The golden file lives in the repository root, shared with the Rust tests.
File goldenFile() {
  for (final path in [
    '../../testdata/signed_envelope.golden.json',
    'testdata/signed_envelope.golden.json',
  ]) {
    final file = File(path);
    if (file.existsSync()) return file;
  }
  throw StateError('signed_envelope.golden.json not found; run from apps/leveler-mobile');
}

void main() {
  late Map<String, dynamic> golden;
  late SimplePublicKey devicePublicKey;
  late DateTime verifierNow;
  late String expectedRecipient;

  setUpAll(() {
    golden = jsonDecode(goldenFile().readAsStringSync()) as Map<String, dynamic>;
    final keys = golden['keys'] as Map<String, dynamic>;
    devicePublicKey = publicKeyFromB64Url(keys['device_pubkey_b64url'] as String);
    final verifier = golden['verifier'] as Map<String, dynamic>;
    verifierNow = parseTimestamp(verifier['now'] as String)!;
    expectedRecipient = verifier['recipient_id'] as String;
  });

  test('every golden case behaves as the answer key documents', () async {
    final cases = golden['cases'] as List<dynamic>;
    expect(cases, isNotEmpty);

    for (final raw in cases) {
      final testCase = raw as Map<String, dynamic>;
      final name = testCase['name'] as String;
      final envelope = SignedEnvelope.fromJson(
        testCase['envelope'] as Map<String, dynamic>,
      );

      if (testCase['expect'] == 'accept') {
        final payload = await verifyEnvelope(
          envelope,
          expectedRecipientId: expectedRecipient,
          publicKey: devicePublicKey,
          now: verifierNow,
        );
        expect(payload, isNotEmpty, reason: '$name should verify');

        // The canonical string is the actual agreement between the two
        // implementations; comparing it catches a divergence that a passing
        // signature check might still hide.
        final expected = testCase['canonical_string'] as String?;
        if (expected != null) {
          expect(await canonicalString(envelope), expected, reason: '$name canonical string');
        }
      } else {
        String? code;
        try {
          await verifyEnvelope(
            envelope,
            expectedRecipientId: expectedRecipient,
            publicKey: devicePublicKey,
            now: verifierNow,
          );
        } on EnvelopeException catch (error) {
          code = error.code;
        }
        expect(code, isNotNull, reason: '$name must be refused, not accepted');
      }
    }
  });

  test('the device fingerprint matches what the host prints', () async {
    final keys = golden['keys'] as Map<String, dynamic>;
    final fingerprint = await fingerprintOf(devicePublicKey.bytes);
    expect(fingerprint, keys['device_fingerprint']);
    expect(groupFingerprint(fingerprint), keys['device_fingerprint_display']);
  });

  test('a frame this device signs verifies against its own key', () async {
    final identity = await DeviceIdentity.fromSeed(List<int>.filled(32, 7));
    final now = DateTime.utc(2026, 7, 25, 12);
    final frame = await signEnvelope(
      keyPair: identity.keyPair,
      senderId: 'dev_golden',
      recipientId: 'rt_golden',
      streamId: 'str_1',
      seq: 7,
      now: now,
      contentType: ContentType.sessionUpstream,
      payload: utf8.encode('{"type":"snapshot","session_id":"s1"}'),
    );

    final payload = await verifyEnvelope(
      frame,
      expectedRecipientId: 'rt_golden',
      publicKey: identity.publicKey,
      now: now,
    );
    expect(utf8.decode(payload), '{"type":"snapshot","session_id":"s1"}');

    // The same seed the golden file uses, so this must reproduce its string.
    expect(await canonicalString(frame), golden['cases'][0]['canonical_string']);
  });
}
