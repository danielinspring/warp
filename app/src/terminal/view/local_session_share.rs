//! TerminalView host UX for local LAN session share (desktop only).

use session_sharing_protocol::common::WindowSize;
use warpui::clipboard::ClipboardContent;
use warpui::{SingletonEntity, ViewContext};

use super::TerminalView;
use crate::features::FeatureFlag;
use crate::terminal::local_session_share::{
    preferred_bind_ip, LocalSessionShareHub, COPY_LOCAL_SHARE_LINK_TEXT, LOCAL_SHARE_ACTIVE_TOAST,
    LOCAL_SHARE_BLOCKS_CLOUD_TOAST, LOCAL_SHARE_CLOUD_BLOCK_TOAST, LOCAL_SHARE_START_FAILED_TOAST,
};
use crate::view_components::DismissibleToast;

impl TerminalView {
    pub(crate) fn start_local_lan_share(&mut self, ctx: &mut ViewContext<Self>) {
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

        let bind_ip = match preferred_bind_ip() {
            Ok(ip) => ip,
            Err(err) => {
                log::warn!("Local LAN share start failed: {err}");
                self.show_local_share_toast(LOCAL_SHARE_START_FAILED_TOAST, ctx);
                return;
            }
        };

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

        if let Some(publisher) = self.local_session_share_hub.event_publisher() {
            self.model.lock().set_local_share_event_publisher(publisher);
        }

        ctx.clipboard()
            .write(ClipboardContent::plain_text(handle.url));
        self.show_local_share_toast(LOCAL_SHARE_ACTIVE_TOAST, ctx);
        ctx.notify();
    }

    pub(crate) fn stop_local_lan_share(&mut self, ctx: &mut ViewContext<Self>) {
        if !self.local_session_share_hub.is_active() {
            return;
        }
        self.local_session_share_hub.stop();
        self.model.lock().clear_local_share_event_publisher();
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

    pub(crate) fn toast_local_share_blocks_cloud(&self, ctx: &mut ViewContext<Self>) {
        self.show_local_share_toast(LOCAL_SHARE_BLOCKS_CLOUD_TOAST, ctx);
    }
}

/// Construct a hub for a new TerminalView (desktop).
pub(crate) fn new_hub() -> LocalSessionShareHub {
    LocalSessionShareHub::new()
}
