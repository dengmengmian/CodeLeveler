/// What the screens talk to: one object that owns the identity, the pairing,
/// the tokens, the socket, and the session it is showing.
///
/// It is where the app's security rules become behaviour rather than intent:
/// nothing reaches a screen unverified, an observe pairing cannot compose, and
/// a stream that fails verification is reported instead of quietly ending.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:cryptography/cryptography.dart';
import 'package:flutter/foundation.dart';

import '../crypto/keys.dart';
import '../crypto/store.dart';
import '../net/relay_client.dart';
import '../net/session_socket.dart';
import '../protocol/commands.dart';
import '../protocol/envelope.dart';
import '../protocol/pairing.dart';
import '../protocol/wire.dart';
import 'session_state.dart';

/// One session in a project, as the list shows it.
class SessionSummary {
  SessionSummary({
    required this.id,
    required this.goal,
    required this.status,
    required this.updatedAt,
  });

  final String id;
  final String goal;
  final String status;
  final String updatedAt;

  static SessionSummary fromJson(Map<String, dynamic> json) => SessionSummary(
        id: json['id'] as String? ?? '',
        goal: json['goal'] as String? ?? '',
        status: json['status'] as String? ?? '',
        updatedAt: json['updated_at'] as String? ?? '',
      );
}

/// One project on the paired host.
class ProjectSummary {
  ProjectSummary({required this.id, required this.display, required this.status});
  final String id;
  final String display;
  final String status;

  bool get isOnline => status == 'online';

  static ProjectSummary fromJson(Map<String, dynamic> json) => ProjectSummary(
        id: json['project_id'] as String? ?? '',
        display: json['path_display'] as String? ?? '',
        status: json['status'] as String? ?? 'offline',
      );
}

/// Named `LinkState` rather than `ConnectionState`: Flutter already exports a
/// type by that name for `AsyncSnapshot`, and two meanings for one identifier in
/// the same file is a bug waiting for a tired reader.
enum LinkState { idle, connecting, online, offline, untrusted }

class AppController extends ChangeNotifier {
  AppController({required Vault vault, RelayClient Function(String baseUrl)? relayFactory})
      : _vault = vault,
        _relayFactory = relayFactory ?? ((baseUrl) => RelayClient(baseUrl: baseUrl));

  final Vault _vault;
  final RelayClient Function(String) _relayFactory;
  final Random _random = Random.secure();

  DeviceIdentity? identity;
  Pairing? pairing;
  SimplePublicKey? _anchoredRuntimeKey;
  RelayClient? _relay;
  SessionTokens? _tokens;

  SessionSocket? _socket;
  StreamSubscription<SocketEvent>? _socketEvents;
  SeqCounter _seq = SeqCounter();

  List<ProjectSummary> projects = [];
  String? currentProjectId;
  SessionState? session;

  /// Sessions in the project currently open, newest activity first.
  List<SessionSummary> sessions = [];
  bool sessionsLoading = false;

  /// Commands sent but not yet acknowledged, oldest first.
  ///
  /// Resent on reconnect *with their original `command_id`*, which is what
  /// makes a retry the same command rather than a second one — the host's
  /// receipt store dedupes on that id. Bounded, because a phone that queued
  /// without limit would eventually replay a long-forgotten instruction into a
  /// session that had moved on.
  final Map<String, DeliverMessage> _unacked = {};
  static const int _maxUnacked = 16;

  LinkState connection = LinkState.idle;

  /// The last thing that went wrong, in words a user can act on.
  String? lastError;

  /// True while the phone has claimed a pairing secret and is waiting for a
  /// person to accept it on the host.
  bool awaitingHostConfirmation = false;

  bool get isPaired => pairing != null;
  bool get isObserveOnly => _tokens?.isObserveOnly ?? false;

  /// Load the identity and pairing this installation already has.
  Future<void> restore() async {
    final seed = await _vault.deviceSeed();
    if (seed == null) {
      final fresh = await DeviceIdentity.generate();
      await _vault.saveDeviceSeed(await fresh.extractSeed());
      identity = fresh;
    } else {
      identity = await DeviceIdentity.fromSeed(seed);
    }

    pairing = await _vault.pairing();
    if (pairing != null) {
      _anchoredRuntimeKey = publicKeyFromB64Url(pairing!.runtimePubkeyB64Url);
      _relay = _relayFactory(pairing!.relayUrl);
    }
    notifyListeners();
  }

  /// Show the user what they must compare on their terminal.
  Future<PairingConfirmation> previewPairing(PairingQrPayload payload) async {
    final anchored = publicKeyFromB64Url(payload.runtimePubkey);
    return PairingConfirmation(
      deviceFingerprint: groupFingerprint(await identity!.fingerprint()),
      hostFingerprint: groupFingerprint(await fingerprintOf(anchored.bytes)),
    );
  }

  /// Claim the secret and wait for the host's accept.
  ///
  /// The pairing is stored only once tokens have been issued, which the relay
  /// does only after the host accepted — so a rejected pairing leaves nothing
  /// behind to be confused with a real one.
  Future<void> completePairing(
    PairingQrPayload payload, {
    required String deviceName,
    required String platform,
    String scope = 'interactive',
  }) async {
    final relay = _relayFactory(payload.relayUrl);
    await relay.completePairing(
      deviceId: identity!.deviceId,
      devicePubkeyB64Url: identity!.publicKeyB64Url(),
      deviceName: deviceName,
      platform: platform,
      pairingSecret: payload.pairingSecret,
      scope: scope,
    );

    // The relay will not issue tokens until a human accepts on the host, so
    // this waits for that rather than failing on the 401 that is the *expected*
    // answer for the next few seconds. Asking first and waiting second is the
    // whole point of the design: the phone cannot promote its own pairing.
    awaitingHostConfirmation = true;
    notifyListeners();
    final tokens = await _awaitHostAcceptance(relay, payload.runtimeId);
    awaitingHostConfirmation = false;

    final stored = Pairing(
      relayUrl: payload.relayUrl,
      runtimeId: payload.runtimeId,
      runtimePubkeyB64Url: payload.runtimePubkey,
      deviceId: identity!.deviceId,
    );
    await _vault.savePairing(stored);
    await _vault.saveRefreshToken(tokens.refreshToken);

    pairing = stored;
    _anchoredRuntimeKey = publicKeyFromB64Url(payload.runtimePubkey);
    _relay = relay;
    _tokens = tokens;
    notifyListeners();
  }

  /// Poll for tokens until the host accepts, the host rejects, or time runs out.
  ///
  /// The host's confirmation window is ten minutes; this gives up sooner,
  /// because a phone showing "waiting" for ten minutes teaches its user that
  /// the app is broken. Giving up is not the same as failing: the pairing is
  /// still claimable on the host, and trying again resumes it.
  Future<SessionTokens> _awaitHostAcceptance(RelayClient relay, String runtimeId) async {
    final deadline = DateTime.now().add(const Duration(minutes: 3));
    RelayException? last;
    while (DateTime.now().isBefore(deadline)) {
      try {
        return await relay.authenticate(
          identity: identity!,
          runtimeId: runtimeId,
          now: DateTime.now(),
          nonce: _nonce(),
        );
      } on RelayException catch (error) {
        // `unauthorized` here means "not accepted yet" — the device record does
        // not exist until the host writes it. `revoked` means the human said
        // no, and waiting longer would be waiting for nothing.
        if (error.code == 'revoked') rethrow;
        if (!error.needsReauth) rethrow;
        last = error;
        await Future<void>.delayed(const Duration(seconds: 2));
      }
    }
    throw last ??
        RelayException(408, 'timeout', '电脑一直没有确认这次配对');
  }

  /// Ensure a usable access token, refreshing or re-authenticating as needed.
  Future<String> _accessToken() async {
    final now = DateTime.now();
    final tokens = _tokens;
    if (tokens != null && !tokens.needsRefresh(now)) return tokens.accessToken;

    final stored = await _vault.refreshToken();
    if (tokens != null && stored != null) {
      try {
        final rotated = await _relay!.refresh(
          identity: identity!,
          refreshToken: stored,
          now: now,
          pairingScope: tokens.pairingScope,
        );
        _tokens = rotated;
        await _vault.saveRefreshToken(rotated.refreshToken);
        return rotated.accessToken;
      } on RelayException catch (error) {
        // A rotated-away token means the relay saw it used twice, which it
        // treats as theft. Re-authenticating with the key is the honest
        // recovery; if that fails too, the device really is unpaired.
        if (!error.needsReauth) rethrow;
      }
    }

    final fresh = await _relay!.authenticate(
      identity: identity!,
      runtimeId: pairing!.runtimeId,
      now: now,
      nonce: _nonce(),
    );
    _tokens = fresh;
    await _vault.saveRefreshToken(fresh.refreshToken);
    return fresh.accessToken;
  }

  /// One signed RPC, verified before it is believed.
  Future<Map<String, dynamic>> _rpc(
    String method, {
    String? projectId,
    Map<String, dynamic> body = const {},
  }) async {
    final token = await _accessToken();
    final rpcId = 'rpc:${_uuid()}';
    final payload = jsonEncode({
      'method': method,
      if (projectId != null) 'project_id': projectId,
      'body': body,
    });
    final request = await signEnvelope(
      keyPair: identity!.keyPair,
      senderId: identity!.deviceId,
      recipientId: pairing!.runtimeId,
      streamId: rpcId,
      seq: 1,
      now: DateTime.now(),
      contentType: ContentType.rpcRequest,
      payload: utf8.encode(payload),
    );

    final answer = await _relay!.rpc(
      accessToken: token,
      hostId: pairing!.runtimeId,
      request: request,
    );
    // The response reuses the request's stream id, and that id is inside the
    // signature — so a relay cannot answer one request with another's result.
    if (answer.streamId != rpcId) {
      throw EnvelopeException(EnvelopeError.invalidFrame, 'answer belongs to another request');
    }
    final verified = await verifyEnvelope(
      answer,
      expectedRecipientId: identity!.deviceId,
      publicKey: _anchoredRuntimeKey!,
      now: DateTime.now(),
    );
    final decoded = jsonDecode(utf8.decode(verified));
    return decoded is Map<String, dynamic> ? decoded : {'value': decoded};
  }

  /// The host's open projects.
  Future<void> loadProjects() async {
    connection = LinkState.connecting;
    lastError = null;
    notifyListeners();
    try {
      final token = await _accessToken();
      final rpcId = 'rpc:${_uuid()}';
      final request = await signEnvelope(
        keyPair: identity!.keyPair,
        senderId: identity!.deviceId,
        recipientId: pairing!.runtimeId,
        streamId: rpcId,
        seq: 1,
        now: DateTime.now(),
        contentType: ContentType.rpcRequest,
        payload: utf8.encode(jsonEncode({'method': 'list_projects', 'body': {}})),
      );
      final answer = await _relay!.rpc(
        accessToken: token,
        hostId: pairing!.runtimeId,
        request: request,
      );
      final verified = await verifyEnvelope(
        answer,
        expectedRecipientId: identity!.deviceId,
        publicKey: _anchoredRuntimeKey!,
        now: DateTime.now(),
      );
      final listed = jsonDecode(utf8.decode(verified)) as List<dynamic>;
      projects = listed
          .map((raw) => ProjectSummary.fromJson(raw as Map<String, dynamic>))
          .toList(growable: false);
      connection = LinkState.online;
    } on RelayException catch (error) {
      connection = error.isTransient ? LinkState.offline : LinkState.idle;
      lastError = _explain(error);
    } on EnvelopeException catch (error) {
      // A list the phone cannot verify is a list the relay may have written.
      connection = LinkState.untrusted;
      lastError = '收到无法验签的项目列表（${error.code}）。已丢弃，不予显示。';
    }
    notifyListeners();
  }

  /// Enter a project: open its stream and ask for the sessions in it.
  ///
  /// The stream is opened first because the session list arrives as a runtime
  /// *event*, not as a reply — there is nowhere else for it to come from.
  Future<void> openProject(String projectId) async {
    await _closeSocket();
    currentProjectId = projectId;
    session = null;
    sessions = [];
    sessionsLoading = true;
    _seq = SeqCounter();
    notifyListeners();

    try {
      final token = await _accessToken();
      final socket = await SessionSocket.connect(
        relayUrl: pairing!.relayUrl,
        hostId: pairing!.runtimeId,
        projectId: projectId,
        accessToken: token,
        deviceId: identity!.deviceId,
        runtimePublicKey: _anchoredRuntimeKey!,
      );
      _socket = socket;
      _socketEvents = socket.events.listen(_onSocketEvent);
      connection = LinkState.online;
      // No session id yet, so this one cannot ride `_deliver`.
      await _send(DeliverMessage(
        commandId: _uuid(),
        sessionId: '',
        command: Commands.requestSessionList(),
      ));
    } on RelayException catch (error) {
      connection = error.isTransient ? LinkState.offline : LinkState.idle;
      lastError = _explain(error);
      sessionsLoading = false;
    } catch (error) {
      // Anything else — a refused WebSocket upgrade, a socket that died mid
      // handshake — must still end the spinner. A screen that says "loading"
      // forever is the least useful way to report a failure.
      connection = LinkState.idle;
      lastError = '打不开这个项目的会话流：$error';
      sessionsLoading = false;
    }
    notifyListeners();
  }

  /// Leave the project list level.
  Future<void> closeProject() async {
    await _closeSocket();
    currentProjectId = null;
    sessions = [];
    session = null;
    notifyListeners();
  }

  /// Create a session in a project and open its stream.
  Future<void> startSession(String projectId, {required String goal}) async {
    final bootstrap = await _rpc(
      'create_session',
      projectId: projectId,
      body: {'goal': goal, 'model': null, 'mode': 'request_approval'},
    );
    final created = bootstrap['session'] as Map<String, dynamic>;
    await openSession(projectId, created['id'] as String, snapshot: created);
  }

  /// Attach to a session: open the stream first, then render the snapshot.
  ///
  /// That order matters. Subscribing after the snapshot would lose whatever the
  /// host said in between, and the phone would show a transcript that is a few
  /// seconds behind with no sign of it.
  Future<void> openSession(
    String projectId,
    String sessionId, {
    Map<String, dynamic>? snapshot,
  }) async {
    session = SessionState(sessionId);

    // Reuse the project's stream when there is one: reconnecting would drop
    // events between the two sockets, and the gap would show up as a
    // transcript that quietly skips a few seconds.
    if (_socket == null || currentProjectId != projectId) {
      await _closeSocket();
      currentProjectId = projectId;
      _seq = SeqCounter();
      final token = await _accessToken();
      final socket = await SessionSocket.connect(
        relayUrl: pairing!.relayUrl,
        hostId: pairing!.runtimeId,
        projectId: projectId,
        accessToken: token,
        deviceId: identity!.deviceId,
        runtimePublicKey: _anchoredRuntimeKey!,
      );
      _socket = socket;
      _socketEvents = socket.events.listen(_onSocketEvent);
    }
    connection = LinkState.online;

    // Tell the host which session this stream is now watching, so its
    // per-session state (and the approval timeout's idea of what we are
    // looking at) follows the user.
    await _send(DeliverMessage(
      commandId: _uuid(),
      sessionId: sessionId,
      command: Commands.openSession(sessionId),
    ));

    if (snapshot != null) {
      session!.applySnapshot(snapshot);
    } else {
      await requestSnapshot();
    }
    await _resendUnacked();
    notifyListeners();
  }

  void _onSocketEvent(SocketEvent event) {
    switch (event) {
      case SocketMessage(message: final message):
        switch (message) {
          case RuntimeEventMessage(event: final runtimeEvent):
            if (runtimeEvent['type'] == 'session_list') {
              sessions = (runtimeEvent['sessions'] as List<dynamic>? ?? const [])
                  .map((raw) => SessionSummary.fromJson(raw as Map<String, dynamic>))
                  .toList(growable: false);
              sessionsLoading = false;
            }
            session?.applyEvent(runtimeEvent);
          case SnapshotMessage(session: final snapshot):
            session?.applySnapshot(snapshot);
          case AckMessage(commandId: final commandId):
            _unacked.remove(commandId);
          case ErrorMessage(code: final code, message: final text, commandId: final failed):
            if (failed != null) _unacked.remove(failed);
            if (code == 'resync_required') {
              session?.markResyncRequired();
              unawaited(requestSnapshot());
            } else {
              lastError = '$code：$text';
            }
          case ResyncRequired():
            session?.markResyncRequired();
            unawaited(requestSnapshot());
          case ProjectStatusMessage():
            unawaited(loadProjects());
          case UnknownDownstream(kind: final kind):
            // Newer host, older app: resynchronise rather than guess.
            session?.applyEvent({'type': kind});
        }
      case SocketFailure(code: final code, detail: final detail):
        if (code == 'stream_closed') {
          connection = LinkState.offline;
          lastError = '与开发机的连接已断开：$detail';
        } else {
          // Verification failed. This is not a network problem, and saying so
          // plainly is the point of having signatures at all.
          connection = LinkState.untrusted;
          lastError = '收到一帧无法验证的数据（$code）。已丢弃。如果反复出现，请撤销配对。';
        }
    }
    notifyListeners();
  }

  /// Leave the current session and go back to the project list.
  ///
  /// A method rather than letting a screen assign the field and poke
  /// `notifyListeners`, which is protected — and which would also skip closing
  /// the socket, leaving a stream open for a screen nobody is looking at.
  Future<void> closeSession() async {
    session = null;
    notifyListeners();
  }

  Future<void> requestSnapshot() async {
    final current = session;
    if (current == null) return;
    await _send(SnapshotRequest(current.sessionId));
  }

  Future<void> submit(String text) async {
    final current = session;
    if (current == null || text.trim().isEmpty) return;
    await _deliver(Commands.submitMessage(sessionId: current.sessionId, content: text));
  }

  Future<void> cancelTurn() async {
    final current = session;
    if (current == null) return;
    await _deliver(Commands.cancelCurrentTurn(current.sessionId));
  }

  Future<void> answerApproval(String requestId, ApprovalChoice choice) =>
      _deliver(Commands.approvalDecision(requestId: requestId, decision: choice));

  Future<void> answerClarification(String requestId, String answer) =>
      _deliver(Commands.answerClarification(requestId: requestId, answer: answer));

  Future<void> _deliver(Map<String, dynamic> command) async {
    final current = session;
    if (current == null) return;
    if (isObserveOnly) {
      // The host would refuse this anyway; refusing here explains why.
      lastError = '这台设备是只读配对，不能发送指令。';
      notifyListeners();
      return;
    }
    if (_unacked.length >= _maxUnacked) {
      lastError = '还有 ${_unacked.length} 条指令没有收到确认，先恢复连接再继续。';
      notifyListeners();
      return;
    }
    final message = DeliverMessage(
      commandId: _uuid(),
      sessionId: current.sessionId,
      command: command,
    );
    _unacked[message.commandId] = message;
    await _send(message);
  }

  /// Re-send what was never acknowledged, after a stream comes back.
  ///
  /// Same ids, so the host treats a duplicate as the command it already has.
  Future<void> _resendUnacked() async {
    for (final message in _unacked.values.toList(growable: false)) {
      await _send(message);
    }
  }

  Future<void> _send(UpstreamMessage message) async {
    final socket = _socket;
    if (socket == null) {
      lastError = '还没有连接到开发机。';
      notifyListeners();
      return;
    }
    final frame = await signEnvelope(
      keyPair: identity!.keyPair,
      senderId: identity!.deviceId,
      recipientId: pairing!.runtimeId,
      streamId: 'str_app',
      seq: _seq.take(),
      now: DateTime.now(),
      contentType: ContentType.sessionUpstream,
      payload: message.encode(),
    );
    socket.send(frame);
  }

  Future<void> _closeSocket() async {
    await _socketEvents?.cancel();
    _socketEvents = null;
    await _socket?.close();
    _socket = null;
  }

  /// Forget this installation's pairing and key.
  Future<void> unpair() async {
    await _closeSocket();
    await _vault.clear();
    pairing = null;
    _tokens = null;
    _anchoredRuntimeKey = null;
    projects = [];
    session = null;
    connection = LinkState.idle;
    // A fresh identity, so re-pairing is a new device rather than a revived one.
    identity = await DeviceIdentity.generate();
    await _vault.saveDeviceSeed(await identity!.extractSeed());
    notifyListeners();
  }

  String _explain(RelayException error) => switch (error.code) {
        'runtime_offline' => '开发机当前不在线。稍后重试。',
        'project_offline' => '这个项目在电脑上没有运行。',
        // Two spellings for one fact, from two layers: the relay says
        // `revoked` when it refuses a token, the host says `device_revoked`
        // when it refuses a frame. A user meeting the second one used to get
        // the raw code.
        'revoked' || 'device_revoked' =>
          '这台设备的配对已在电脑上被撤销。要继续使用，请在设置里清除配对后重新配对。',
        'unauthorized' => '授权已失效，请重新配对。',
        'rate_limited' => '请求太频繁，请稍候。',
        _ => '${error.code}：${error.message}',
      };

  String _nonce() => _randomToken(16);

  String _uuid() => _randomToken(16);

  String _randomToken(int bytes) {
    final buffer = List<int>.generate(bytes, (_) => _random.nextInt(256));
    return base64Url.encode(buffer).replaceAll('=', '');
  }

  @override
  void dispose() {
    unawaited(_closeSocket());
    _relay?.close();
    super.dispose();
  }
}
