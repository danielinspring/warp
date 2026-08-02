//! session-sharing-protocol helpers for the local LAN share hub.

use session_sharing_protocol::common::{
    ActivePrompt, AgentPromptFailureReason, BlockId, CommandExecutionRequestId,
    ControlActionFailureReason, InputReplicaId, OrderedTerminalEvent, OrderedTerminalEventType,
    ParticipantId, ParticipantList, PresentViewer, ProfileData, Role, Scrollback, Viewer,
    WindowSize,
};
use session_sharing_protocol::sharer::{LegacySessionSourceType, SessionSourceType};
use session_sharing_protocol::viewer::{DownstreamMessage, UpstreamMessage};

/// A mutating request from a local-share guest, forwarded to the host pane.
#[derive(Debug, Clone)]
pub enum LocalShareGuestRequest {
    ExecuteCommand {
        participant_id: ParticipantId,
        command: String,
    },
    WriteToPty {
        participant_id: ParticipantId,
        bytes: Vec<u8>,
    },
}

/// Builds an interactive [`DownstreamMessage::JoinedSuccessfully`] for a newly
/// connected guest. Guests join as [`Role::Executor`] so they can run commands
/// and write to long-running / alt-screen PTYs. `scrollback` is the host
/// snapshot taken at share start.
#[allow(deprecated)]
pub(crate) fn joined_successfully(
    window_size: WindowSize,
    scrollback: Scrollback,
) -> (DownstreamMessage, ParticipantId) {
    let viewer_id = ParticipantId::new();
    let input_replica_id = InputReplicaId::from(uuid::Uuid::new_v4().to_string());
    let mut profile = ProfileData::default();
    profile.input_replica_id = input_replica_id.clone();

    let viewer_info = session_sharing_protocol::common::ParticipantInfo {
        id: viewer_id.clone(),
        profile_data: profile.clone(),
        selection: Default::default(),
    };

    let message = DownstreamMessage::JoinedSuccessfully {
        scrollback: Box::new(scrollback),
        active_prompt: ActivePrompt::PS1,
        latest_event_no: None,
        window_size,
        participant_list: Box::new(ParticipantList {
            viewers: vec![Viewer {
                info: viewer_info.clone(),
                role: Role::Executor,
                is_present: true,
            }],
            present_viewers: vec![PresentViewer {
                info: viewer_info,
                max_acl: Role::Executor,
            }],
            ..Default::default()
        }),
        viewer_id: viewer_id.clone(),
        viewer_firebase_uid: String::new(),
        init_block_id: BlockId::from(uuid::Uuid::new_v4().to_string()),
        input_replica_id,
        universal_developer_input_context: None,
        source_type: LegacySessionSourceType::User,
        detailed_source_type: SessionSourceType::User,
        source_task_id: None,
    };
    (message, viewer_id)
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

/// How the WS handler should react to a post-join upstream message.
pub(crate) enum UpstreamDisposition {
    /// Send this reply to the guest and do nothing else.
    Reply(DownstreamMessage),
    /// Forward a mutating request to the host; optionally also ack the guest.
    GuestRequest {
        request: LocalShareGuestRequest,
        ack: Option<DownstreamMessage>,
    },
    /// No reply and no host effect.
    Ignore,
}

/// Handles a guest upstream message after join.
pub(crate) fn handle_upstream(
    message: UpstreamMessage,
    viewer_id: &ParticipantId,
) -> UpstreamDisposition {
    match message {
        UpstreamMessage::Initialize(_) => UpstreamDisposition::Ignore,
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
        | UpstreamMessage::UpdatePendingUserRole { .. } => UpstreamDisposition::Ignore,
        UpstreamMessage::ExecuteCommand { command, .. } => {
            let request_id = CommandExecutionRequestId::new();
            UpstreamDisposition::GuestRequest {
                request: LocalShareGuestRequest::ExecuteCommand {
                    participant_id: viewer_id.clone(),
                    command,
                },
                ack: Some(DownstreamMessage::CommandExecutionRequestInFlight(
                    request_id,
                )),
            }
        }
        UpstreamMessage::WriteToPty { bytes, .. } => UpstreamDisposition::GuestRequest {
            request: LocalShareGuestRequest::WriteToPty {
                participant_id: viewer_id.clone(),
                bytes,
            },
            ack: None,
        },
        UpstreamMessage::SendAgentPrompt(_) => {
            UpstreamDisposition::Reply(DownstreamMessage::AgentPromptRequestFailed {
                reason: AgentPromptFailureReason::InsufficientPermissions,
            })
        }
        UpstreamMessage::SendControlAction(_) => {
            UpstreamDisposition::Reply(DownstreamMessage::ControlActionRequestFailed {
                reason: ControlActionFailureReason::InsufficientPermissions,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_successfully_is_executor_with_empty_scrollback() {
        let (msg, viewer_id) = joined_successfully(
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
            viewer_id: joined_viewer_id,
            ..
        } = msg
        else {
            panic!("expected JoinedSuccessfully");
        };
        assert!(scrollback.blocks.is_empty());
        assert_eq!(window_size.num_rows, 24);
        assert_eq!(window_size.num_cols, 80);
        assert_eq!(joined_viewer_id, viewer_id);
        assert!(participant_list
            .viewers
            .iter()
            .all(|viewer| viewer.role == Role::Executor));
        assert!(participant_list
            .present_viewers
            .iter()
            .all(|viewer| viewer.max_acl == Role::Executor));
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
        let (msg, _) = joined_successfully(
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
    fn execute_command_becomes_guest_request() {
        let viewer_id = ParticipantId::new();
        let disposition = handle_upstream(
            UpstreamMessage::ExecuteCommand {
                buffer_id: session_sharing_protocol::common::BufferId::from("buf".to_string()),
                command: "echo hi".to_string(),
            },
            &viewer_id,
        );
        match disposition {
            UpstreamDisposition::GuestRequest {
                request:
                    LocalShareGuestRequest::ExecuteCommand {
                        participant_id,
                        command,
                    },
                ack: Some(DownstreamMessage::CommandExecutionRequestInFlight(_)),
            } => {
                assert_eq!(participant_id, viewer_id);
                assert_eq!(command, "echo hi");
            }
            _ => panic!("unexpected disposition for ExecuteCommand"),
        }
    }

    #[test]
    fn write_to_pty_becomes_guest_request() {
        let viewer_id = ParticipantId::new();
        let disposition = handle_upstream(
            UpstreamMessage::WriteToPty {
                request_id: session_sharing_protocol::common::WriteToPtyRequestId {
                    participant_id: viewer_id.clone(),
                    op_no: session_sharing_protocol::common::WriteToPtySeqNo::zero(),
                },
                bytes: b"x".to_vec(),
            },
            &viewer_id,
        );
        match disposition {
            UpstreamDisposition::GuestRequest {
                request: LocalShareGuestRequest::WriteToPty { bytes, .. },
                ack: None,
            } => assert_eq!(bytes, b"x"),
            _ => panic!("unexpected disposition for WriteToPty"),
        }
    }

    #[test]
    fn agent_prompt_is_still_rejected() {
        use session_sharing_protocol::common::AgentPromptRequest;

        // Agent control stays host-only for local share.
        let disposition = handle_upstream(
            UpstreamMessage::SendAgentPrompt(AgentPromptRequest {
                id: Default::default(),
                server_conversation_token: None,
                prompt: "hi".to_string(),
                attachments: vec![],
            }),
            &ParticipantId::new(),
        );
        assert!(matches!(
            disposition,
            UpstreamDisposition::Reply(DownstreamMessage::AgentPromptRequestFailed {
                reason: AgentPromptFailureReason::InsufficientPermissions
            })
        ));
    }
}
