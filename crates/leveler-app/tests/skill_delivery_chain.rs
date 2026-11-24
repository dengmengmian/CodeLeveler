//! The skill-delivery audit's client chain: engine fact → runtime projection.
use leveler_app::event_bridge::EventBridge;
use leveler_client_protocol::{RuntimeEvent, UiSkillDeliveryFailure, UiSkillScope, UiSkillSource};
use leveler_engine::EngineEvent;
use leveler_skills::{
    SkillDeliveryAudit, SkillDeliveryEntry, SkillDeliveryFailure, SkillDeliverySurface, SkillScope,
    SkillSource,
};
use tokio::sync::broadcast;

fn loaded() -> SkillDeliveryEntry {
    SkillDeliveryEntry {
        name: "deploy".into(),
        requested_as: "/deploy".into(),
        scope: Some(SkillScope::Project),
        source: Some(SkillSource::Native),
        bytes: 2048,
        failure: None,
    }
}

fn refused() -> SkillDeliveryEntry {
    SkillDeliveryEntry {
        name: "release".into(),
        requested_as: "$release".into(),
        scope: None,
        source: None,
        bytes: 0,
        failure: Some(SkillDeliveryFailure::Disabled),
    }
}

/// The bridge stays alive in the caller: dropping the last sender would turn
/// an empty channel into `Closed` and hide the fact that nothing was sent.
fn deliver(audit: SkillDeliveryAudit) -> (broadcast::Receiver<RuntimeEvent>, EventBridge) {
    let (sender, receiver) = broadcast::channel(16);
    let mut bridge = EventBridge::new(sender);
    bridge.forward(EngineEvent::SkillDelivery { audit });
    (receiver, bridge)
}

#[test]
fn a_selected_skill_reaches_the_client_with_provenance_and_outcome() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::Mixed,
        discovered: 4,
        index_bytes: 160,
        entries: vec![loaded(), refused()],
        injected_bytes: 2600,
        blocked: None,
        ..Default::default()
    });
    let RuntimeEvent::SkillDelivery { delivery } = events.try_recv().expect("one projection")
    else {
        panic!("the audit must project as a skill delivery");
    };
    assert_eq!(delivery.entries.len(), 2);
    assert_eq!(delivery.entries[0].name, "deploy");
    assert_eq!(delivery.entries[0].requested_as, "/deploy");
    assert_eq!(delivery.entries[0].scope, Some(UiSkillScope::Project));
    assert_eq!(delivery.entries[0].source, Some(UiSkillSource::Native));
    assert_eq!(delivery.entries[0].failure, None);
    assert_eq!(delivery.entries[1].requested_as, "$release");
    assert_eq!(
        delivery.entries[1].failure,
        Some(UiSkillDeliveryFailure::Disabled)
    );
    assert_eq!(delivery.blocked, None);
}

#[test]
fn an_index_only_turn_sends_nothing_to_the_status_line() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::IndexOnly,
        discovered: 4,
        index_bytes: 160,
        entries: Vec::new(),
        injected_bytes: 0,
        blocked: None,
        ..Default::default()
    });
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn a_mention_only_success_needs_no_extra_line() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::Mention,
        discovered: 1,
        index_bytes: 40,
        entries: vec![SkillDeliveryEntry {
            requested_as: "$deploy".into(),
            ..loaded()
        }],
        injected_bytes: 2048,
        blocked: None,
        ..Default::default()
    });
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn a_mention_refusal_is_still_reported() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::Mention,
        discovered: 1,
        index_bytes: 40,
        entries: vec![refused()],
        injected_bytes: 0,
        blocked: None,
        ..Default::default()
    });
    let RuntimeEvent::SkillDelivery { delivery } = events.try_recv().expect("one projection")
    else {
        panic!("a refusal must project regardless of surface");
    };
    assert_eq!(
        delivery.entries[0].failure,
        Some(UiSkillDeliveryFailure::Disabled)
    );
}

#[test]
fn an_host_block_reaches_the_client_even_with_no_entry() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::Slash,
        discovered: 0,
        index_bytes: 0,
        entries: Vec::new(),
        injected_bytes: 0,
        blocked: Some(SkillDeliveryFailure::CapabilityUnavailable),
        ..Default::default()
    });
    let RuntimeEvent::SkillDelivery { delivery } = events.try_recv().expect("one projection")
    else {
        panic!("a block must project as a skill delivery");
    };
    assert!(delivery.entries.is_empty());
    assert_eq!(
        delivery.blocked,
        Some(UiSkillDeliveryFailure::CapabilityUnavailable)
    );
}

#[test]
fn the_projection_is_body_free() {
    let (mut events, _bridge) = deliver(SkillDeliveryAudit {
        surface: SkillDeliverySurface::Slash,
        discovered: 1,
        index_bytes: 40,
        entries: vec![loaded()],
        injected_bytes: 2048,
        blocked: None,
        ..Default::default()
    });
    let event = events.try_recv().expect("one projection");
    let wire = serde_json::to_string(&event).unwrap();
    assert!(!wire.contains("procedure-body"));
    assert!(!wire.contains("/private/host/path"));
    assert!(wire.contains("\"skill_delivery\""));
}
