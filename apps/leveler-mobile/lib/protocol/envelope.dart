/// The signed envelope, as the phone must build and check it.
///
/// This file is the app's half of a two-language agreement. The host's Rust
/// implementation and this one must produce byte-identical canonical strings,
/// or every signature disagrees and nothing works — so the rules are restated
/// here rather than paraphrased, and `test/envelope_golden_test.dart` replays
/// the same answer key the Rust tests use.
///
/// What the phone must never do is act on an unverified frame. A relay carries
/// these envelopes and is assumed hostile: it can drop, delay, reorder and
/// duplicate them, and the only reason it cannot *author* one is that the
/// signature covers the header as well as the body.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:cryptography/cryptography.dart';

import 'ids.dart';

/// Seconds a frame's timestamp may differ from the receiver's clock.
const int timestampWindowSeconds = 120;

/// The envelope version this build speaks.
const String envelopeVersion = '1';

enum FrameSender {
  device,
  runtime;

  String get wire => name;

  static FrameSender? fromWire(String value) {
    for (final sender in FrameSender.values) {
      if (sender.wire == value) return sender;
    }
    return null;
  }
}

enum ContentType {
  sessionUpstream('session_upstream'),
  sessionDownstream('session_downstream'),
  rpcRequest('rpc_request'),
  rpcResponse('rpc_response');

  const ContentType(this.wire);
  final String wire;

  static ContentType? fromWire(String value) {
    for (final type in ContentType.values) {
      if (type.wire == value) return type;
    }
    return null;
  }
}

/// Why a frame was refused. The codes match the host's catalogue so the two
/// sides can be read together in a bug report.
enum EnvelopeError {
  invalidFrame('invalid_frame'),
  recipientMismatch('recipient_mismatch'),
  signatureInvalid('signature_invalid'),
  staleTimestamp('stale_timestamp');

  const EnvelopeError(this.code);
  final String code;
}

class EnvelopeException implements Exception {
  EnvelopeException(this.error, [this.detail]);
  final EnvelopeError error;
  final String? detail;

  String get code => error.code;

  @override
  String toString() => 'EnvelopeException(${error.code}${detail == null ? '' : ': $detail'})';
}

/// One envelope, exactly as it travels.
class SignedEnvelope {
  SignedEnvelope({
    required this.version,
    required this.sender,
    required this.senderId,
    required this.recipientId,
    required this.streamId,
    required this.seq,
    required this.ts,
    required this.contentType,
    required this.payloadB64,
    required this.sigB64,
  });

  final String version;
  final FrameSender sender;
  final String senderId;
  final String recipientId;
  final String streamId;
  final int seq;
  final String ts;
  final ContentType contentType;
  final String payloadB64;
  final String sigB64;

  factory SignedEnvelope.fromJson(Map<String, dynamic> json) {
    final sender = FrameSender.fromWire(json['sender'] as String? ?? '');
    final contentType = ContentType.fromWire(json['content_type'] as String? ?? '');
    final seq = json['seq'];
    if (sender == null || contentType == null || seq is! int) {
      throw EnvelopeException(EnvelopeError.invalidFrame, 'unrecognised envelope fields');
    }
    return SignedEnvelope(
      // Serialised as a number by the host; compared as its decimal text.
      version: '${json['v']}',
      sender: sender,
      senderId: json['sender_id'] as String? ?? '',
      recipientId: json['recipient_id'] as String? ?? '',
      streamId: json['stream_id'] as String? ?? '',
      seq: seq,
      ts: json['ts'] as String? ?? '',
      contentType: contentType,
      payloadB64: json['payload_b64'] as String? ?? '',
      sigB64: json['sig_b64'] as String? ?? '',
    );
  }

  Map<String, dynamic> toJson() => {
        'v': int.parse(version),
        'sender': sender.wire,
        'sender_id': senderId,
        'recipient_id': recipientId,
        'stream_id': streamId,
        'seq': seq,
        'ts': ts,
        'content_type': contentType.wire,
        'payload_b64': payloadB64,
        'sig_b64': sigB64,
      };

  /// The raw payload bytes. Standard base64, with padding.
  Uint8List payloadBytes() {
    try {
      return Uint8List.fromList(base64.decode(payloadB64));
    } on FormatException {
      throw EnvelopeException(EnvelopeError.invalidFrame, 'payload is not base64');
    }
  }

  /// The payload decoded as JSON, once the signature has been checked.
  Map<String, dynamic> payloadJson() {
    final decoded = jsonDecode(utf8.decode(payloadBytes()));
    if (decoded is! Map<String, dynamic>) {
      throw EnvelopeException(EnvelopeError.invalidFrame, 'payload is not a JSON object');
    }
    return decoded;
  }
}

/// Build the exact string a signature covers.
///
/// Every id is checked first: a separator inside one would shift the field
/// boundaries, which is precisely how two different frames could be made to
/// share a signature.
Future<String> canonicalString(SignedEnvelope envelope) async {
  for (final id in [envelope.senderId, envelope.recipientId, envelope.streamId]) {
    if (!isValidId(id)) {
      throw EnvelopeException(EnvelopeError.invalidFrame, 'illegal id: $id');
    }
  }
  if (parseTimestamp(envelope.ts) == null) {
    throw EnvelopeException(EnvelopeError.invalidFrame, 'timestamp must be YYYY-MM-DDTHH:MM:SSZ');
  }
  if (envelope.seq < 0) {
    throw EnvelopeException(EnvelopeError.invalidFrame, 'seq must not be negative');
  }

  // The digest is over the *raw payload bytes*, not over their base64 text.
  final digest = await Sha256().hash(envelope.payloadBytes());
  final digestHex = digest.bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();

  return [
    envelope.version,
    envelope.sender.wire,
    envelope.senderId,
    envelope.recipientId,
    envelope.streamId,
    '${envelope.seq}',
    envelope.ts,
    envelope.contentType.wire,
    digestHex,
  ].join('|');
}

/// Parse the one accepted timestamp shape, or null.
///
/// Deliberately strict: accepting other ISO-8601 forms would let two peers
/// disagree about the bytes being signed while both believing they parsed the
/// same instant.
DateTime? parseTimestamp(String ts) {
  final pattern = RegExp(r'^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z$');
  final match = pattern.firstMatch(ts);
  if (match == null) return null;
  try {
    return DateTime.utc(
      int.parse(match.group(1)!),
      int.parse(match.group(2)!),
      int.parse(match.group(3)!),
      int.parse(match.group(4)!),
      int.parse(match.group(5)!),
      int.parse(match.group(6)!),
    );
  } on ArgumentError {
    return null;
  }
}

/// Format an instant the way every signer must.
String formatTimestamp(DateTime instant) {
  final utc = instant.toUtc();
  String two(int value) => value.toString().padLeft(2, '0');
  return '${utc.year.toString().padLeft(4, '0')}-${two(utc.month)}-${two(utc.day)}'
      'T${two(utc.hour)}:${two(utc.minute)}:${two(utc.second)}Z';
}

/// Verify a frame and return its payload bytes.
///
/// Throws rather than returning null: a caller that forgot to check a boolean
/// would be acting on relay-controlled data, and that is the one mistake this
/// whole design exists to prevent.
Future<Uint8List> verifyEnvelope(
  SignedEnvelope envelope, {
  required String expectedRecipientId,
  required SimplePublicKey publicKey,
  required DateTime now,
}) async {
  // Audience first: a genuine frame addressed to another host is still not for
  // us, and checking it before the signature keeps the cheap rejection cheap.
  if (envelope.recipientId != expectedRecipientId) {
    throw EnvelopeException(EnvelopeError.recipientMismatch,
        'addressed to ${envelope.recipientId}');
  }

  final canonical = await canonicalString(envelope);

  final Uint8List signature;
  try {
    signature = Uint8List.fromList(base64.decode(envelope.sigB64));
  } on FormatException {
    throw EnvelopeException(EnvelopeError.invalidFrame, 'signature is not base64');
  }

  final verified = await Ed25519().verify(
    utf8.encode(canonical),
    signature: Signature(signature, publicKey: publicKey),
  );
  if (!verified) {
    throw EnvelopeException(EnvelopeError.signatureInvalid);
  }

  // Freshness last, so a stale frame is still reported as stale rather than as
  // a signature problem — the two mean very different things to a user.
  final stamped = parseTimestamp(envelope.ts)!;
  final skew = now.toUtc().difference(stamped).inSeconds.abs();
  if (skew > timestampWindowSeconds) {
    throw EnvelopeException(EnvelopeError.staleTimestamp, '${skew}s from this clock');
  }

  return envelope.payloadBytes();
}

/// Sign one payload as this device.
Future<SignedEnvelope> signEnvelope({
  required SimpleKeyPair keyPair,
  required String senderId,
  required String recipientId,
  required String streamId,
  required int seq,
  required DateTime now,
  required ContentType contentType,
  required List<int> payload,
}) async {
  final envelope = SignedEnvelope(
    version: envelopeVersion,
    sender: FrameSender.device,
    senderId: senderId,
    recipientId: recipientId,
    streamId: streamId,
    seq: seq,
    ts: formatTimestamp(now),
    contentType: contentType,
    payloadB64: base64.encode(payload),
    sigB64: '',
  );
  final canonical = await canonicalString(envelope);
  final signature = await Ed25519().sign(utf8.encode(canonical), keyPair: keyPair);
  return SignedEnvelope(
    version: envelope.version,
    sender: envelope.sender,
    senderId: envelope.senderId,
    recipientId: envelope.recipientId,
    streamId: envelope.streamId,
    seq: envelope.seq,
    ts: envelope.ts,
    contentType: envelope.contentType,
    payloadB64: envelope.payloadB64,
    sigB64: base64.encode(signature.bytes),
  );
}
