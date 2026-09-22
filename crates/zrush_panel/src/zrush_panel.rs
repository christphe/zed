use anyhow::{Context as _, Result};
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Pixels, Render, WeakEntity, Window, actions, div, px,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Label, ListItem, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::{RevealTarget, SwitchWorktree};
use zrush_core::{
    agent,
    app::Zrush,
    config::{Config, Dirs},
    error::ZrushError,
    host::Host,
    model::{
        Session,
        tree::{self, NodeId, Row, RowKind, TreeInput},
    },
};

actions!(zrush_panel, [Toggle, Refresh]);

#[derive(Debug)]
enum HostRequest {
    OpenWorkspace(PathBuf),
    RunAgent {
        cwd: PathBuf,
        command: Vec<String>,
    },
}

#[derive(Clone, Default)]
struct ZedHost {
    requests: Arc<Mutex<Vec<HostRequest>>>,
}

impl ZedHost {
    fn drain(&self) -> Result<Vec<HostRequest>> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| ZrushError::msg("Zed host request queue is poisoned"))?;
        Ok(requests.drain(..).collect())
    }
}

impl Host for ZedHost {
    fn open_workspace(&self, path: &std::path::Path) -> zrush_core::error::Result<()> {
        self.requests
            .lock()
            .map_err(|_| ZrushError::msg("Zed host request queue is poisoned"))?
            .push(HostRequest::OpenWorkspace(path.to_path_buf()));
        Ok(())
    }

    fn run_agent(
        &self,
        cwd: &std::path::Path,
        command: &[String],
    ) -> zrush_core::error::Result<()> {
        self.requests
            .lock()
            .map_err(|_| ZrushError::msg("Zed host request queue is poisoned"))?
            .push(HostRequest::RunAgent {
                cwd: cwd.to_path_buf(),
                command: command.to_vec(),
            });
        Ok(())
    }
}


pub struct ZrushPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: WeakEntity<Workspace>,
    service: Option<Arc<Zrush>>,
    host: ZedHost,
    rows: Vec<Row>,
    sessions: Vec<Session>,
    collapsed: HashSet<NodeId>,
    expanded: HashMap<NodeId, usize>,
    error: Option<String>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            workspace.toggle_panel_focus::<ZrushPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Refresh, _window, cx| {
            if let Some(panel) = workspace.panel::<ZrushPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.refresh();
                    cx.notify();
                });
            }
        });
    })
    .detach();
}

impl ZrushPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let repo = workspace
            .update_in(&mut cx, |workspace, _window, cx| {
                workspace
                    .root_paths(cx)
                    .into_iter()
                    .next()
                    .map(|path| path.as_ref().to_path_buf())
            })?
            .context("Zrush needs a local project root")?;

        let host = ZedHost::default();
        let (service, error) = match Self::service_for(repo, host.clone()) {
            Ok(service) => (Some(Arc::new(service)), None),
            Err(error) => (None, Some(error.to_string())),
        };

        workspace.update_in(&mut cx, |_workspace, _window, cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
                workspace: workspace.clone(),
                service,
                host,
                rows: Vec::new(),
                sessions: Vec::new(),
                collapsed: HashSet::new(),
                expanded: HashMap::new(),
                error,
            };
            panel.refresh();
            cx.new(|_| panel)
        })
    }

    fn service_for(repo: PathBuf, host: ZedHost) -> Result<Zrush> {
        let dirs = Dirs::from_env()?;
        let cfg = Config::load(&dirs)?;
        Ok(Zrush::new(
            dirs,
            cfg,
            repo,
            agent::available(),
            Box::new(host),
            None,
        )?)
    }

    fn refresh(&mut self) {
        let Some(service) = self.service.as_ref() else {
            return;
        };

        match Self::load_rows(service, &self.collapsed, &self.expanded) {
            Ok((rows, sessions)) => {
                self.rows = rows;
                self.sessions = sessions;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn load_rows(
        service: &Zrush,
        collapsed: &HashSet<NodeId>,
        expanded: &HashMap<NodeId, usize>,
    ) -> Result<(Vec<Row>, Vec<Session>)> {
        let worktrees = service.worktrees()?;
        let live = service.live_sessions();
        let resumable = service.resumable_sessions(&worktrees, &live, false);
        let sessions: Vec<Session> = live.into_iter().chain(resumable).collect();

        let assigned = tree::assign(
            &sessions,
            &worktrees,
            service.main_root(),
            &service.new_root(),
        );

        let statuses = worktrees
            .iter()
            .filter_map(|worktree| {
                service
                    .status(&worktree.path)
                    .ok()
                    .map(|status| (worktree.path.clone(), status))
            })
            .collect::<HashMap<_, _>>();

        let history = worktrees
            .iter()
            .map(|worktree| {
                (
                    worktree.path.clone(),
                    service.history_count(&worktree.path),
                )
            })
            .collect::<HashMap<_, _>>();

        let rows = tree::build(&TreeInput {
            worktrees: &worktrees,
            assigned: &assigned,
            statuses: &statuses,
            history: &history,
            collapsed,
            expanded,
            resumable_max: service.config().resumable_max,
            show_all: false,
        });

        Ok((rows, sessions))
    }

    fn spawn_agent_terminal(
        &mut self,
        cwd: PathBuf,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((program, args)) = command.split_first() else {
            self.error = Some("agent returned an empty command".into());
            cx.notify();
            return;
        };

        let Some(workspace) = self.workspace.upgrade() else {
            self.error = Some("Zed workspace is no longer available".into());
            cx.notify();
            return;
        };

        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            self.error = Some("Zed terminal panel is unavailable".into());
            cx.notify();
            return;
        };

        let label = format!("zrush · {}", program);
        let spawn = SpawnInTerminal {
            id: TaskId(format!("zrush:{}:{}", cwd.display(), program)),
            full_label: label.clone(),
            label: label.clone(),
            command: Some(program.clone()),
            args: args.to_vec(),
            command_label: command.join(" "),
            cwd: Some(cwd),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            show_summary: false,
            show_command: false,
            show_rerun: false,
            ..Default::default()
        };

        terminal_panel
            .update(cx, |panel, cx| panel.spawn_task(&spawn, window, cx))
            .detach();
    }

    fn expand_more(&mut self, row: &Row, cx: &mut Context<Self>) {
        let step = self
            .service
            .as_ref()
            .map(|service| service.config().more_step)
            .unwrap_or(20);
        let current = self
            .expanded
            .get(&row.node)
            .copied()
            .or_else(|| {
                self.service
                    .as_ref()
                    .map(|service| service.config().resumable_max)
            })
            .unwrap_or(5);
        self.expanded.insert(row.node.clone(), current + step);
        self.refresh();
        cx.notify();
    }

    fn activate_row(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        if row.kind == RowKind::More {
            self.expand_more(row, cx);
            return;
        }

        let Some(service) = self.service.as_ref() else {
            return;
        };

        let NodeId::Worktree(path) = &row.node else {
            return;
        };

        let binding = row.session_id.as_deref().and_then(|id| {
            self.sessions
                .iter()
                .find(|session| session.id == id)
                .map(|session| (session.agent, session.id.clone()))
        });
        let binding_ref = binding
            .as_ref()
            .map(|(agent, id)| (*agent, id.as_str()));

        if let Err(err) = service.open(path, binding_ref) {
            self.error = Some(err.to_string());
            cx.notify();
            return;
        }

        if let Some((agent, id)) = binding.as_ref()
            && let Err(err) = service.run_agent(path, agent, Some(id))
        {
            self.error = Some(err.to_string());
            cx.notify();
            return;
        }

        let requests = match self.host.drain() {
            Ok(requests) => requests,
            Err(err) => {
                self.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };

        for request in requests {
            match request {
                HostRequest::OpenWorkspace(path) => {
                    let display_name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    window.dispatch_action(
                        Box::new(SwitchWorktree {
                            path,
                            display_name,
                        }),
                        cx,
                    );
                }
                HostRequest::RunAgent { cwd, command } => {
                    self.spawn_agent_terminal(cwd, command, window, cx);
                }
            }
        }
    }

    fn render_row(
        &self,
        index: usize,
        row: Row,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let clickable = row.kind == RowKind::More
            || (matches!(row.node, NodeId::Worktree(_))
                && !matches!(row.kind, RowKind::Orphan | RowKind::Orphans));

        div()
            .id(("zrush-row", index))
            .cursor_pointer()
            .when(clickable, |element| {
                let clicked = row.clone();
                element.on_click(cx.listener(
                    move |panel, event: &ClickEvent, window, cx| {
                        if event.is_right_click() || event.first_focus() {
                            return;
                        }
                        panel.activate_row(&clicked, window, cx);
                    },
                ))
            })
            .child(
                ListItem::new(("zrush-list-item", index)).child(
                    v_flex()
                        .w_full()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap_2()
                                .child(
                                    Label::new(format!("{}{}", row.glyph, row.label))
                                        .size(LabelSize::Small),
                                )
                                .when(!row.badge.is_empty(), |line| {
                                    line.child(
                                        Label::new(row.badge.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .when(!row.status.is_empty(), |item| {
                            item.child(
                                Label::new(row.status.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .when(
                            matches!(row.kind, RowKind::Worktree) && !row.location.is_empty(),
                            |item| {
                                item.child(
                                    Label::new(row.location.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            },
                        ),
                ),
            )
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
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
        let header = h_flex()
            .w_full()
            .justify_between()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("ZRUSH").size(LabelSize::Small))
            .child(
                Label::new(format!("{} rows", self.rows.len()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );

        let body = if let Some(error) = self.error.clone() {
            v_flex()
                .p_2()
                .gap_1()
                .child(Label::new("Zrush unavailable").size(LabelSize::Small))
                .child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else if self.rows.is_empty() {
            v_flex()
                .p_2()
                .child(
                    Label::new("No worktrees or sessions")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .id("zrush-rows")
                .size_full()
                .overflow_y_scroll()
                .children(
                    self.rows
                        .clone()
                        .into_iter()
                        .enumerate()
                        .map(|(index, row)| self.render_row(index, row, cx)),
                )
                .into_any_element()
        };

        v_flex()
            .id("zrush-panel")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .child(header)
            .child(body)
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
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::Code)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Zrush")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(Toggle)
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}
