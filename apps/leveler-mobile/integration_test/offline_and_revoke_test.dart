/// What the phone shows when the other end goes away.
///
/// Two states a user meets and cannot act on: a project whose daemon is not
/// running, and a pairing the developer revoked. Both are proven in Rust — the
/// agent isolates an offline project, the relay drops a revoked device's tokens
/// at once — and neither says anything about what the phone puts on screen. A
/// correct 401 that the app renders as an endless spinner is the same failure
/// as no 401 at all.
///
/// The host stops one project's daemon before this runs, and revokes this
/// device partway through. Run it through `scripts/simulator_pairing.sh`.
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:leveler_mobile/crypto/store.dart';

import 'harness.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('an offline project is shown as offline, and a revoke is explained',
      (tester) async {
    final controller = await pairAndReachProjects(
      tester,
      store: persistPairing ? KeystoreSecretStore() : MemorySecretStore(),
    );

    // ---- One project up, one down. ----
    //
    // The offline one must still be *listed*. Dropping it would tell the user
    // their repository is gone when the only thing gone is a daemon.
    expect(controller.projects.length, greaterThanOrEqualTo(2),
        reason: '电脑上要有两个项目（一个已停）才能验这一步：'
            '${controller.projects.map((p) => "${p.display}(${p.status})").join(", ")}');
    final offline = controller.projects.where((project) => !project.isOnline).toList();
    final online = controller.projects.where((project) => project.isOnline).toList();
    expect(offline, isNotEmpty, reason: '应当有一个项目是离线的');
    expect(online, isNotEmpty, reason: '应当还有一个项目在线');

    await bringIntoView(tester, find.text(offline.first.display));
    expect(find.text('离线（电脑上未运行）'), findsWidgets,
        reason: '离线项目要说明白为什么进不去，屏幕上是：${visibleText(tester)}');

    // ---- The host revokes this device. ----
    //
    // The script does it on its own clock, so this waits for the app to notice
    // rather than for a number of seconds to pass. Refreshing the list is what
    // a user does when a screen looks stale, and it is the request that will
    // come back 401.
    var noticed = false;
    final deadline = DateTime.now().add(const Duration(seconds: 90));
    while (DateTime.now().isBefore(deadline) && !noticed) {
      await controller.loadProjects();
      await tester.pump(const Duration(milliseconds: 300));
      await Future<void>.delayed(const Duration(seconds: 2));
      noticed = controller.lastError != null;
    }

    expect(noticed, isTrue, reason: '撤销之后再取项目列表本该失败，却成功了');
    // Words a user can act on, not a status code. "revoked" is the one case
    // where trying again is pointless, so the message has to say so.
    expect(
      controller.lastError,
      anyOf(contains('撤销'), contains('重新配对')),
      reason: '撤销后应当说清楚发生了什么：${controller.lastError}',
    );
    await tester.pumpAndSettle();
    expect(find.textContaining(RegExp('撤销|重新配对')), findsWidgets,
        reason: '这句话要真的显示在屏幕上，屏幕上是：${visibleText(tester)}');

    // The key is *not* wiped automatically. A transient failure must never cost
    // a user their pairing; forgetting is a deliberate act, on the settings
    // screen, which is why the pairing is still here.
    expect(controller.isPaired, isTrue,
        reason: '一次失败不该自动清掉配对——那会把网络抖动变成重新配对');
  });
}
