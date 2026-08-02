//! TerminalView host UX for local LAN session share (desktop only).

use std::net::{IpAddr, Ipv4Addr};

use chrono::Local;
use session_sharing_protocol::common::WindowSize;
use warpui::clipboard::ClipboardContent;
use warpui::{AppContext, SingletonEntity, ViewContext};

use super::{
    InlineBannerItem, InlineBannerType, SharedSessionBanners, TerminalAction, TerminalView,
};
use crate::features::FeatureFlag;
use crate::menu::{MenuItem, MenuItemFields};
use crate::terminal::local_session_share::{
    all_interfaces_label, bind_candidate_label, is_all_interfaces, non_loopback_candidates,
    resolve_palette_bind_ip, LocalSessionShareHub, LocalShareGuestRequest,
    COPY_LOCAL_SHARE_LINK_TEXT, LOCAL_SHARE_ACTIVE_TOAST, LOCAL_SHARE_ALL_INTERFACES_WARNING,
    LOCAL_SHARE_BLOCKS_CLOUD_TOAST, LOCAL_SHARE_CLOUD_BLOCK_TOAST, LOCAL_SHARE_LITE_VIEWER_TOAST,
    LOCAL_SHARE_ROTATED_TOAST, LOCAL_SHARE_START_FAILED_TOAST,
};
use crate::view_components::DismissibleToast;

impl TerminalView {
    /// Command Palette entry: resolve a bind address and start (or re-copy).
    pub(crate) fn start_local_lan_share(&mut self, ctx: &mut ViewContext<Self>) {
        if !FeatureFlag::LocalLanSessionShare.is_enabled() {
            return;
        }

        if self.local_session_share_hub.is_active() {
            self.copy_local_lan_share_link(ctx);
            return;
        }

        let (bind_ip, label) = match resolve_palette_bind_ip() {
            Ok(resolved) => resolved,
            Err(err) => {
                log::warn!("Local LAN share start failed: {err}");
                self.show_local_share_toast(LOCAL_SHARE_START_FAILED_TOAST, ctx);
                return;
            }
        };

        self.start_local_lan_share_with_bind(bind_ip, Some(label), ctx);
    }

    /// Start (or re-copy) using an explicit bind address from the pane menu.
    pub(crate) fn start_local_lan_share_with_bind(
        &mut self,
        bind_ip: IpAddr,
        bind_label: Option<String>,
        ctx: &mut ViewContext<Self>,
    ) {
        if !FeatureFlag::LocalLanSessionShare.is_enabled() {
            return;
        }

        if self.local_session_share_hub.is_active() {
            self.copy_local_lan_share_link(ctx);
            return;
        }

        {
            let model = self.model.lock();
            if model.shared_session_status().is_sharer_or_viewer() {
                self.show_local_share_toast(LOCAL_SHARE_CLOUD_BLOCK_TOAST, ctx);
                return;
            }
        }

        let handle = match self.local_session_share_hub.start(bind_ip, 0) {
            Ok(handle) => handle,
            Err(err) => {
                log::warn!("Local LAN share bind/start failed: {err}");
                self.show_local_share_toast(LOCAL_SHARE_START_FAILED_TOAST, ctx);
                return;
            }
        };

        let window_size = WindowSize {
            num_rows: self.size_info.rows(),
            num_cols: self.size_info.columns(),
        };
        if let Err(err) = self.local_session_share_hub.set_window_size(window_size) {
            log::warn!("Failed to set local LAN share window size: {err}");
        }

        {
            let scrollback =
                crate::terminal::shared_session::local_share_scrollback(&self.model.lock());
            // Guests only ever see this snapshot for pre-share history, so log
            // its size (never its contents) to make an empty one diagnosable.
            log::info!(
                "Local LAN share scrollback snapshot: {} blocks, {} bytes",
                scrollback.blocks.len(),
                scrollback.num_bytes().as_u64()
            );
            if let Err(err) = self.local_session_share_hub.set_scrollback(scrollback) {
                log::warn!("Failed to set local LAN share scrollback: {err}");
            }
        }

        if let Some(publisher) = self.local_session_share_hub.event_publisher() {
            self.model.lock().set_local_share_event_publisher(publisher);
        }

        if let Some(guest_rx) = self.local_session_share_hub.take_guest_request_receiver() {
            self.listen_for_local_share_guest_requests(guest_rx, ctx);
        }

        // Mirror whatever is already in the input editor so a guest that joins
        // immediately sees in-progress typing, not just post-share keystrokes.
        self.publish_local_share_typed_input(ctx);

        self.insert_local_lan_share_started_banner(ctx);

        ctx.clipboard()
            .write(ClipboardContent::plain_text(handle.url));

        if is_all_interfaces(bind_ip) {
            self.show_local_share_toast(LOCAL_SHARE_ALL_INTERFACES_WARNING, ctx);
        } else if !handle.has_wasm_viewer {
            self.show_local_share_toast(LOCAL_SHARE_LITE_VIEWER_TOAST, ctx);
        } else if let Some(label) = bind_label {
            self.show_local_share_toast(&format!("Local network share active on {label}"), ctx);
        } else {
            self.show_local_share_toast(LOCAL_SHARE_ACTIVE_TOAST, ctx);
        }

        self.refresh_local_share_pane_header(ctx);
        ctx.notify();
    }

    pub(crate) fn stop_local_lan_share(&mut self, ctx: &mut ViewContext<Self>) {
        if !self.local_session_share_hub.is_active() {
            return;
        }
        self.local_session_share_hub.stop();
        self.model.lock().clear_local_share_event_publisher();
        self.insert_local_lan_share_ended_banner(ctx);
        self.refresh_local_share_pane_header(ctx);
        ctx.notify();
    }

    pub(crate) fn copy_local_lan_share_link(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(handle) = self.local_session_share_hub.current_handle() else {
            return;
        };
        ctx.clipboard()
            .write(ClipboardContent::plain_text(handle.url));
        self.show_local_share_toast(COPY_LOCAL_SHARE_LINK_TEXT, ctx);
    }

    pub(crate) fn rotate_local_lan_share_link(&mut self, ctx: &mut ViewContext<Self>) {
        if !FeatureFlag::LocalLanSessionShare.is_enabled() {
            return;
        }
        match self.local_session_share_hub.rotate_secret() {
            Ok(handle) => {
                ctx.clipboard()
                    .write(ClipboardContent::plain_text(handle.url));
                self.show_local_share_toast(LOCAL_SHARE_ROTATED_TOAST, ctx);
            }
            Err(err) => {
                log::warn!("Failed to rotate local LAN share secret: {err}");
            }
        }
    }

    /// Pane-overflow menu items to start a share on a chosen interface.
    ///
    /// Note: pane header overflow does not support [`MenuItem::Header`] /
    /// [`MenuItem::Submenu`] (it panics), so labels are inlined on each item.
    pub(crate) fn local_lan_share_bind_menu_items() -> Vec<MenuItem<TerminalAction>> {
        let mut items = Vec::new();
        let candidates = non_loopback_candidates();
        if candidates.is_empty() {
            return items;
        }

        for candidate in &candidates {
            items.push(
                MenuItemFields::new(format!(
                    "Start local share on {}",
                    bind_candidate_label(candidate)
                ))
                .with_on_select_action(TerminalAction::StartLocalLanShareWithBind {
                    bind_ip: candidate.addr,
                })
                .into_item(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            MenuItemFields::new(format!("Start local share — {}", all_interfaces_label()))
                .with_on_select_action(TerminalAction::StartLocalLanShareWithBind {
                    bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                })
                .into_item(),
        );
        items
    }

    fn insert_local_lan_share_started_banner(&mut self, ctx: &mut ViewContext<Self>) {
        let banner_id = self.inline_banners_state.next_banner_id();
        let started_at = Local::now();

        let mut model = self.model.lock();
        if let SharedSessionBanners::LastShared {
            started_banner_id,
            ended_banner_id,
            ..
        } = self.inline_banners_state.local_lan_share_banner_state
        {
            model
                .block_list_mut()
                .remove_inline_banner(started_banner_id);
            model.block_list_mut().remove_inline_banner(ended_banner_id);
        }

        self.inline_banners_state.local_lan_share_banner_state =
            SharedSessionBanners::ActiveShare {
                started_banner_id: banner_id,
                started_at,
                is_remote_control: false,
            };

        model
            .block_list_mut()
            .append_inline_banner(InlineBannerItem::new(
                banner_id,
                InlineBannerType::LocalLanShareStart,
            ));
        ctx.notify();
    }

    fn insert_local_lan_share_ended_banner(&mut self, ctx: &mut ViewContext<Self>) {
        let banner_id = self.inline_banners_state.next_banner_id();
        let banner = InlineBannerItem::new(banner_id, InlineBannerType::LocalLanShareEnd);

        if let SharedSessionBanners::ActiveShare {
            started_banner_id,
            started_at,
            is_remote_control,
        } = self.inline_banners_state.local_lan_share_banner_state
        {
            self.inline_banners_state.local_lan_share_banner_state =
                SharedSessionBanners::LastShared {
                    started_banner_id,
                    started_at,
                    is_remote_control,
                    ended_at: Local::now(),
                    ended_banner_id: banner_id,
                };
        }

        self.model
            .lock()
            .block_list_mut()
            .append_inline_banner(banner);
        ctx.notify();
    }

    fn refresh_local_share_pane_header(&mut self, ctx: &mut ViewContext<Self>) {
        self.pane_configuration.update(ctx, |pane_config, ctx| {
            pane_config.refresh_pane_header_overflow_menu_items(ctx);
            pane_config.notify_header_content_changed(ctx);
        });
        self.use_agent_footer.update(ctx, |footer, ctx| {
            footer.notify_and_notify_children(ctx);
        });
    }

    fn show_local_share_toast(&self, message: &str, ctx: &mut ViewContext<Self>) {
        let window_id = ctx.window_id();
        crate::workspace::ToastStack::handle(ctx).update(ctx, |toast_stack, ctx| {
            let toast = DismissibleToast::default(message.to_string());
            toast_stack.add_ephemeral_toast(toast, window_id, ctx);
        });
    }

    /// True when local LAN share is active on this pane (for cloud mutual exclusion).
    pub(crate) fn is_local_lan_share_active(&self) -> bool {
        self.local_session_share_hub.is_active()
    }

    /// Publishes the current Warp input-editor text to local-share guests so
    /// typing is mirrored in the lite viewer before Enter.
    pub(crate) fn publish_local_share_typed_input(&self, ctx: &AppContext) {
        let Some(publisher) = self.local_session_share_hub.event_publisher() else {
            return;
        };
        let text = self.input().as_ref(ctx).buffer_text(ctx);
        if let Err(err) = publisher.publish_typed_input(text) {
            log::warn!("Failed to publish local LAN share typed input: {err}");
        }
    }

    /// Spawns a recursive listener that applies guest ExecuteCommand / WriteToPty
    /// requests on the UI thread. Ends when the share stops and the channel closes.
    fn listen_for_local_share_guest_requests(
        &mut self,
        guest_rx: async_channel::Receiver<LocalShareGuestRequest>,
        ctx: &mut ViewContext<Self>,
    ) {
        let next_rx = guest_rx.clone();
        ctx.spawn(
            async move { guest_rx.recv().await },
            move |me, result, ctx| {
                let Ok(request) = result else {
                    return;
                };
                me.apply_local_share_guest_request(request, ctx);
                me.listen_for_local_share_guest_requests(next_rx, ctx);
            },
        );
    }

    fn apply_local_share_guest_request(
        &mut self,
        request: LocalShareGuestRequest,
        ctx: &mut ViewContext<Self>,
    ) {
        if !self.local_session_share_hub.is_active() {
            return;
        }
        match request {
            LocalShareGuestRequest::ExecuteCommand {
                participant_id,
                command,
            } => {
                let is_long_running = self
                    .model
                    .lock()
                    .block_list()
                    .active_block()
                    .is_active_and_long_running();
                if is_long_running {
                    log::info!(
                        "Ignoring local-share ExecuteCommand while a long-running command is active"
                    );
                    return;
                }
                self.input().update(ctx, |input, ctx| {
                    input.try_execute_command_on_behalf_of_shared_session_participant(
                        &command,
                        participant_id,
                        false,
                        ctx,
                    );
                });
            }
            LocalShareGuestRequest::WriteToPty { bytes, .. } => {
                let allow_write = {
                    let model = self.model.lock();
                    model.is_alt_screen_active()
                        || model
                            .block_list()
                            .active_block()
                            .is_active_and_long_running()
                };
                if !allow_write {
                    log::info!(
                        "Ignoring local-share WriteToPty while the host is at an idle prompt"
                    );
                    return;
                }
                self.write_viewer_bytes_to_pty(bytes, ctx);
            }
        }
    }

    pub(crate) fn toast_local_share_blocks_cloud(&self, ctx: &mut ViewContext<Self>) {
        self.show_local_share_toast(LOCAL_SHARE_BLOCKS_CLOUD_TOAST, ctx);
    }
}

/// Construct a hub for a new TerminalView (desktop).
pub(crate) fn new_hub() -> LocalSessionShareHub {
    LocalSessionShareHub::new()
}
