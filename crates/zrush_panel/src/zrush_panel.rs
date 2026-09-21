use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, WeakEntity, Window, actions, div, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zrush_core as _;

actions!(zrush_panel, [Toggle]);

pub struct ZrushPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            workspace.toggle_panel_focus::<ZrushPanel>(window, cx);
        });
    })
    .detach();
}

impl ZrushPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
            })
        })
    }
}

impl EventEmitter<PanelEvent> for ZrushPanel {}

impl Focusable for ZrushPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ZrushPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("zrush-panel")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .p_2()
            .child("ZRUSH")
    }
}

impl Panel for ZrushPanel {
    fn persistent_name() -> &'static str {
        "Zrush"
    }

    fn panel_key() -> &'static str {
        "ZrushPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.position = position;
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        None
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Zrush")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(Toggle)
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}
