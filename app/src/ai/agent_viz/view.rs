//! Pane-backing view for the agent office visualization.
//!
//! Subscribes to the [`local_runtime_event_bus`], folds each
//! [`RuntimeEvent`] into [`AgentVizModel`], and re-seeds a read-only
//! [`CodeEditorView`] with the rendered snapshot.

use warp_editor::content::buffer::InitialBufferState;
use warp_editor::render::element::VerticalExpansionBehavior;
use warp_util::path::LineAndColumnArg;
use warpui::elements::{ChildView, MouseStateHandle};
use warpui::text_layout::ClipConfig;
use warpui::ui_components::components::UiComponent;
use warpui::{
    AppContext, Element, Entity, ModelHandle, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use crate::ai::agent_viz::model::AgentVizModel;
use crate::ai::agent_viz::render;
use crate::ai::local_runtime_event_bus::{self, RunScopedEvent};
use crate::ai::local_runtime_spec::{self, McpServerInfo, SkillInfo};
use crate::appearance::Appearance;
use crate::code::editor::scroll::{ScrollPosition, ScrollTrigger};
use crate::code::editor::view::{CodeEditorRenderOptions, CodeEditorView};
use crate::editor::InteractionState;
use crate::pane_group::focus_state::PaneFocusHandle;
use crate::pane_group::pane::view::{self, HeaderContent, StandardHeader, StandardHeaderOptions};
use crate::pane_group::{BackingView, PaneConfiguration, PaneEvent, PaneHeaderAction};
use crate::ui_components::buttons::icon_button_with_color;
use crate::ui_components::{blended_colors, icons};

pub const AGENT_VIZ_HEADER_TEXT: &str = "Agent office";

const REFRESH_TOOLTIP: &str = "Refresh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentVizViewEvent {
    Pane(PaneEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentVizViewAction {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentVizViewCustomAction {
    Refresh,
}

pub struct AgentVizView {
    editor: ViewHandle<CodeEditorView>,
    model: AgentVizModel,
    pane_configuration: ModelHandle<PaneConfiguration>,
    focus_handle: Option<PaneFocusHandle>,
    refresh_button_mouse_state: MouseStateHandle,
}

impl AgentVizView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let pane_configuration =
            ctx.add_model(|_ctx| PaneConfiguration::new(AGENT_VIZ_HEADER_TEXT));

        let model = AgentVizModel::default();
        let snapshot = Self::render_snapshot(&model, ctx);

        let editor = ctx.add_typed_action_view(|ctx| {
            let mut view = CodeEditorView::new(
                None,
                None,
                CodeEditorRenderOptions::new(VerticalExpansionBehavior::FillMaxHeight),
                ctx,
            );
            Self::apply_snapshot_to_editor(&mut view, &snapshot, ctx);
            view.set_interaction_state(InteractionState::Selectable, ctx);
            view
        });

        // Pump bus events into this view. The pump task ends when the view
        // (and therefore the receiver in `spawn_stream_local`) is dropped.
        let bus_rx = local_runtime_event_bus::subscribe_local();
        ctx.spawn_stream_local(bus_rx, Self::on_runtime_event, |_, _| {});

        Self {
            editor,
            model,
            pane_configuration,
            focus_handle: None,
            refresh_button_mouse_state: MouseStateHandle::default(),
        }
    }

    pub fn pane_configuration(&self) -> ModelHandle<PaneConfiguration> {
        self.pane_configuration.clone()
    }

    pub fn focus(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.focus(&self.editor);
    }

    pub fn reload_snapshot(&mut self, ctx: &mut ViewContext<Self>) {
        let snapshot = Self::render_snapshot(&self.model, ctx);
        self.editor.update(ctx, |view, ctx| {
            Self::apply_snapshot_to_editor(view, &snapshot, ctx);
        });
    }

    fn on_runtime_event(&mut self, scoped: RunScopedEvent, ctx: &mut ViewContext<Self>) {
        self.model.apply(&scoped.run_id, &scoped.event);
        let snapshot = Self::render_snapshot(&self.model, ctx);
        self.editor.update(ctx, |view, ctx| {
            Self::apply_snapshot_to_editor(view, &snapshot, ctx);
        });
    }

    fn render_snapshot(model: &AgentVizModel, ctx: &AppContext) -> String {
        let mcp: Vec<McpServerInfo> = local_runtime_spec::local_mcp_servers(ctx);
        let skills: Vec<SkillInfo> = local_runtime_spec::local_skills(ctx);
        render::render_snapshot(model, &mcp, &skills, local_runtime_spec::local_tools)
    }

    fn apply_snapshot_to_editor(
        view: &mut CodeEditorView,
        snapshot: &str,
        ctx: &mut ViewContext<CodeEditorView>,
    ) {
        let state = InitialBufferState::plain_text(snapshot);
        view.reset(state, ctx);
        let version = view.buffer_version(ctx);
        view.set_pending_scroll(ScrollTrigger::new(
            ScrollPosition::LineAndColumn(LineAndColumnArg {
                line_num: 1,
                column_num: Some(0),
            }),
            version,
        ));
    }

    fn render_refresh_button(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let ui_builder = appearance.ui_builder().clone();

        icon_button_with_color(
            appearance,
            icons::Icon::Refresh,
            false,
            self.refresh_button_mouse_state.clone(),
            blended_colors::text_sub(theme, theme.background()).into(),
        )
        .with_tooltip(move || {
            ui_builder
                .tool_tip(REFRESH_TOOLTIP.to_string())
                .build()
                .finish()
        })
        .build()
        .on_click(|ctx, _, _| {
            ctx.dispatch_typed_action::<PaneHeaderAction<
                AgentVizViewAction,
                AgentVizViewCustomAction,
            >>(PaneHeaderAction::CustomAction(
                AgentVizViewCustomAction::Refresh,
            ));
        })
        .finish()
    }
}

impl Entity for AgentVizView {
    type Event = AgentVizViewEvent;
}

impl View for AgentVizView {
    fn ui_name() -> &'static str {
        "AgentVizView"
    }

    fn render(&self, _app: &AppContext) -> Box<dyn Element> {
        ChildView::new(&self.editor).finish()
    }
}

impl TypedActionView for AgentVizView {
    type Action = AgentVizViewAction;

    fn handle_action(&mut self, _action: &Self::Action, _ctx: &mut ViewContext<Self>) {}
}

impl BackingView for AgentVizView {
    type PaneHeaderOverflowMenuAction = AgentVizViewAction;
    type CustomAction = AgentVizViewCustomAction;
    type AssociatedData = ();

    fn handle_pane_header_overflow_menu_action(
        &mut self,
        _action: &Self::PaneHeaderOverflowMenuAction,
        _ctx: &mut ViewContext<Self>,
    ) {
    }

    fn handle_custom_action(
        &mut self,
        custom_action: &Self::CustomAction,
        ctx: &mut ViewContext<Self>,
    ) {
        match custom_action {
            AgentVizViewCustomAction::Refresh => self.reload_snapshot(ctx),
        }
    }

    fn close(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.emit(AgentVizViewEvent::Pane(PaneEvent::Close));
    }

    fn focus_contents(&mut self, ctx: &mut ViewContext<Self>) {
        self.focus(ctx);
    }

    fn render_header_content(
        &self,
        _ctx: &view::HeaderRenderContext<'_>,
        app: &AppContext,
    ) -> HeaderContent {
        HeaderContent::Standard(StandardHeader {
            title: AGENT_VIZ_HEADER_TEXT.to_string(),
            title_secondary: None,
            title_style: None,
            title_clip_config: ClipConfig::start(),
            title_max_width: None,
            left_of_title: None,
            right_of_title: None,
            left_of_overflow: Some(self.render_refresh_button(app)),
            options: StandardHeaderOptions {
                always_show_icons: true,
                ..StandardHeaderOptions::default()
            },
        })
    }

    fn set_focus_handle(&mut self, focus_handle: PaneFocusHandle, _ctx: &mut ViewContext<Self>) {
        self.focus_handle = Some(focus_handle);
    }
}
