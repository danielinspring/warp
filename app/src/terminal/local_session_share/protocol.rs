//! session-sharing-protocol helpers for the local LAN share hub.

use session_sharing_protocol::common::{
    ActivePrompt, AgentPromptFailureReason, BlockId, CommandExecutionFailureReason,
    CommandExecutionRequestId, ControlActionFailureReason, InputReplicaId, OrderedTerminalEvent,
    OrderedTerminalEventType, ParticipantId, ParticipantList, PresentViewer, ProfileData, Role,
    Scrollback, Viewer, WindowSize, WriteToPtyFailureReason,
};
use session_sharing_protocol::sharer::{LegacySessionSourceType, SessionSourceType};
use session_sharing_protocol::viewer::{DownstreamMessage, UpstreamMessage};

/// Builds a view-only [`DownstreamMessage::JoinedSuccessfully`] for a newly
/// connected guest (PRODUCT.md P15 — Reader only). `scrollback` is the host
/// snapshot taken at share start (PRODUCT.md P20).
#[allow(deprecated)]
pub(crate) fn joined_successfully(
    window_size: WindowSize,
    scrollback: Scrollback,
) -> DownstreamMessage {
    let viewer_id = ParticipantId::new();
    let input_replica_id = InputReplicaId::from(uuid::Uuid::new_v4().to_string());
    let mut profile = ProfileData::default();
    profile.input_replica_id = input_replica_id.clone();

    let viewer_info = session_sharing_protocol::common::ParticipantInfo {
        id: viewer_id.clone(),
        profile_data: profile.clone(),
        selection: Default::default(),
    };

    DownstreamMessage::JoinedSuccessfully {
        scrollback: Box::new(scrollback),
        active_prompt: ActivePrompt::PS1,
        latest_event_no: None,
        window_size,
        participant_list: Box::new(ParticipantList {
            viewers: vec![Viewer {
                info: viewer_info.clone(),
                role: Role::Reader,
                is_present: true,
            }],
            present_viewers: vec![PresentViewer {
                info: viewer_info,
                max_acl: Role::Reader,
            }],
            ..Default::default()
        }),
        viewer_id,
        viewer_firebase_uid: String::new(),
        init_block_id: BlockId::from(uuid::Uuid::new_v4().to_string()),
        input_replica_id,
        universal_developer_input_context: None,
        source_type: LegacySessionSourceType::User,
        detailed_source_type: SessionSourceType::User,
        source_task_id: None,
    }
}

/// LZ4 size-prepended compression matching the cloud sharer path.
pub(crate) fn compress_pty_bytes(bytes: &[u8]) -> Vec<u8> {
    lz4_flex::block::compress_prepend_size(bytes)
}

pub(crate) fn ordered_event_downstream(
    event_no: usize,
    event_type: OrderedTerminalEventType,
) -> DownstreamMessage {
    DownstreamMessage::OrderedTerminalEvent(OrderedTerminalEvent {
        event_no,
        event_type,
    })
}

/// Handles a guest upstream message after join. Returns an optional reply to
/// send back to that guest. Mutating requests always get a permissions failure
/// (Reader-only local share).
pub(crate) fn reply_for_upstream(message: UpstreamMessage) -> Option<DownstreamMessage> {
    match message {
        UpstreamMessage::Initialize(_) => {
            // Initialize is handled before join; ignore duplicates.
            None
        }
        UpstreamMessage::Ping { .. }
        | UpstreamMessage::UpdateSelection(_)
        | UpstreamMessage::UpdateInput(_)
        | UpstreamMessage::Reauthenticated { .. }
        | UpstreamMessage::ReportTerminalSize { .. }
        | UpstreamMessage::UpdateUniversalDeveloperInputContext(_)
        | UpstreamMessage::RequestRole(_)
        | UpstreamMessage::CancelRoleRequest(_)
        | UpstreamMessage::UpdateLinkAccessLevel { .. }
        | UpstreamMessage::UpdateTeamAccessLevel { .. }
        | UpstreamMessage::AddGuests { .. }
        | UpstreamMessage::RemoveGuest { .. }
        | UpstreamMessage::RemovePendingGuest { .. }
        | UpstreamMessage::UpdateUserRole { .. }
        | UpstreamMessage::UpdatePendingUserRole { .. } => None,
        UpstreamMessage::WriteToPty { .. } => Some(DownstreamMessage::WriteToPtyRequestFailed {
            reason: WriteToPtyFailureReason::InsufficientPermissions,
        }),
        UpstreamMessage::ExecuteCommand { .. } => {
            Some(DownstreamMessage::CommandExecutionRequestFailed {
                id: CommandExecutionRequestId::new(),
                reason: CommandExecutionFailureReason::InsufficientPermissions,
            })
        }
        UpstreamMessage::SendAgentPrompt(_) => Some(DownstreamMessage::AgentPromptRequestFailed {
            reason: AgentPromptFailureReason::InsufficientPermissions,
        }),
        UpstreamMessage::SendControlAction(_) => {
            Some(DownstreamMessage::ControlActionRequestFailed {
                reason: ControlActionFailureReason::InsufficientPermissions,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_successfully_is_reader_with_empty_scrollback() {
        let msg = joined_successfully(
            WindowSize {
                num_rows: 24,
                num_cols: 80,
            },
            Scrollback {
                blocks: vec![],
                is_alt_screen_active: false,
            },
        );
        let DownstreamMessage::JoinedSuccessfully {
            scrollback,
            window_size,
            participant_list,
            ..
        } = msg
        else {
            panic!("expected JoinedSuccessfully");
        };
        assert!(scrollback.blocks.is_empty());
        assert_eq!(window_size.num_rows, 24);
        assert_eq!(window_size.num_cols, 80);
        assert!(participant_list
            .viewers
            .iter()
            .all(|viewer| viewer.role == Role::Reader));
        assert!(participant_list
            .present_viewers
            .iter()
            .all(|viewer| viewer.max_acl == Role::Reader));
    }

    #[test]
    fn joined_successfully_preserves_provided_scrollback() {
        use session_sharing_protocol::common::ScrollbackBlock;

        let scrollback = Scrollback {
            blocks: vec![ScrollbackBlock {
                raw: br#"{"test":true}"#.to_vec(),
            }],
            is_alt_screen_active: true,
        };
        let msg = joined_successfully(
            WindowSize {
                num_rows: 24,
                num_cols: 80,
            },
            scrollback,
        );
        let DownstreamMessage::JoinedSuccessfully { scrollback, .. } = msg else {
            panic!("expected JoinedSuccessfully");
        };
        assert_eq!(scrollback.blocks.len(), 1);
        assert!(scrollback.is_alt_screen_active);
    }

    #[test]
    fn compress_round_trips() {
        let raw = b"hello from host pty";
        let compressed = compress_pty_bytes(raw);
        let decompressed = lz4_flex::block::decompress_size_prepended(&compressed).unwrap();
        assert_eq!(decompressed, raw);
    }

    #[test]
    fn write_to_pty_is_rejected() {
        let reply = reply_for_upstream(UpstreamMessage::WriteToPty {
            request_id: session_sharing_protocol::common::WriteToPtyRequestId {
                participant_id: ParticipantId::new(),
                op_no: session_sharing_protocol::common::WriteToPtySeqNo::zero(),
            },
            bytes: b"x".to_vec(),
        });
        assert!(matches!(
            reply,
            Some(DownstreamMessage::WriteToPtyRequestFailed {
                reason: WriteToPtyFailureReason::InsufficientPermissions
            })
        ));
    }
}
