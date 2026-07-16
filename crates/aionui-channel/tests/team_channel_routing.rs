use aionui_channel::message_service::{TeamChannelTarget, team_channel_route_from_extra};

#[test]
fn team_channel_route_detects_lead_conversation() {
    let route = team_channel_route_from_extra(r#"{"teamId":"team-1","slot_id":"lead-1","role":"lead"}"#)
        .expect("team-owned conversation should produce a channel route");

    assert_eq!(route.team_id, "team-1");
    assert_eq!(route.target, TeamChannelTarget::Lead);
}

#[test]
fn team_channel_route_detects_specific_agent_conversation() {
    let route = team_channel_route_from_extra(r#"{"teamId":"team-1","slot_id":"risk-1","role":"teammate"}"#)
        .expect("team-owned teammate conversation should produce a channel route");

    assert_eq!(route.team_id, "team-1");
    assert_eq!(
        route.target,
        TeamChannelTarget::Agent {
            slot_id: "risk-1".into()
        }
    );
}

#[test]
fn team_channel_route_ignores_personal_conversation() {
    assert!(team_channel_route_from_extra(r#"{"session_mode":"yolo"}"#).is_none());
}
