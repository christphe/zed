use anyhow::Result;
use gpui::{
    DismissEvent,
    Global,
    MouseButton,
    MouseDownEvent,
    Point,
    PromptLevel,
    Subscription,
    Task,
    anchored,
    deferred,
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Pixels, Render, WeakEntity, Window, actions, div, px,
};
use std::{
    time::Duration,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::TerminalView;
use settings::{DockSide, RegisterSetting, Settings};
use ui::{ContextMenu, Label, ListItem, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::{RevealTarget, SwitchWorktree};
use zrush_core::{
    model::GitStatus,
    agent,
    app::Zrush,
    config::{Config, Dirs},
    error::ZrushError,
    host::Host,
    model::{
        Session, SessionKind,
        tree::{self, NodeId, Row, RowKind, TreeInput},
    },
};

const AUTO_REFRESH_INTERVAL_SECS: u64 = 10;

/// The panel's side lives in the settings because `Dock::add_panel` only
/// relocates a panel from its `SettingsStore` observer, which re-reads
/// `Panel::position`. Storing it anywhere else leaves the menu entry inert.
#[derive(Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct ZrushPanelSettings {
    pub dock: DockSide,
}

impl Settings for ZrushPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            dock: content.zrush_panel.clone().unwrap().dock.unwrap(),
        }
    }
}

actions!(
    zrush_panel,
    [
        /// Toggles focus on the Zrush panel.
        Toggle,
        /// Reloads the worktrees and sessions shown in the Zrush panel.
        Refresh
    ]
);

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
    workspace: WeakEntity<Workspace>,
    service: Option<Arc<Zrush>>,
    host: ZedHost,
    rows: Vec<Row>,
    sessions: Vec<Session>,
    /// Kept structured so each piece of a worktree's git state can be coloured
    /// on its own; `Row::status` only carries the pre-formatted badge.
    statuses: HashMap<PathBuf, GitStatus>,
    collapsed: HashSet<NodeId>,
    expanded: HashMap<NodeId, usize>,
    /// Raised by something the user did. Sticky: a background refresh must not
    /// wipe a message before it has been read.
    error: Option<String>,
    /// Raised by the last refresh, and cleared by the next one that succeeds.
    load_error: Option<String>,
    /// Whether this panel is the one on show in its dock. The timer keeps
    /// ticking either way, but a hidden panel does no work.
    active: bool,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    refresh_task: Option<Task<()>>,
    auto_refresh: Option<Task<()>>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            workspace.toggle_panel_focus::<ZrushPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Refresh, _window, cx| {
            if let Some(panel) = workspace.panel::<ZrushPanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        });
    })
    .detach();
}

/// Which agent a row runs, and which session it resumes. A row bound to a
/// session resumes that session's own agent; an unbound worktree row starts a
/// fresh session on whichever agent is active. With no agent installed zrush
/// stays a plain worktree router.
/// Identifies the terminal a given agent command owns. Keyed on the full
/// command, not just the program, so resuming two different sessions from the
/// same worktree does not collide on one terminal.
/// An agent run waiting for the workspace that a worktree switch is building.
/// Switching replaces the workspace, so the panel that asked for the run is
/// torn down before it could start it; the panel of the new workspace picks the
/// request up in [`ZrushPanel::load`].
#[derive(Default)]
struct PendingAgentRun(Option<PendingRun>);

struct PendingRun {
    worktree: PathBuf,
    cwd: PathBuf,
    command: Vec<String>,
}

impl Global for PendingAgentRun {}

impl PendingAgentRun {
    fn set(cx: &mut App, worktree: PathBuf, cwd: PathBuf, command: Vec<String>) {
        cx.set_global(PendingAgentRun(Some(PendingRun {
            worktree,
            cwd,
            command,
        })));
    }

    fn clear(cx: &mut App) {
        cx.set_global(PendingAgentRun(None));
    }

    /// Takes the request, and only when it was meant for `root`: a workspace
    /// opened for any other reason must not inherit it. Destructive, so a
    /// request can fire at most once.
    fn take_for(cx: &mut App, root: &std::path::Path) -> Option<(PathBuf, Vec<String>)> {
        let pending = cx.try_global::<PendingAgentRun>()?.0.as_ref()?;
        if pending.worktree != root {
            return None;
        }
        let pending = cx.global_mut::<PendingAgentRun>().0.take()?;
        Some((pending.cwd, pending.command))
    }
}

fn zrush_task_id(cwd: &std::path::Path, command: &[String]) -> TaskId {
    TaskId(format!("zrush:{}:{}", cwd.display(), command.join(" ")))
}

/// The pieces of a worktree's git state, each with the colour that carries its
/// meaning. Built from the structured status rather than `GitStatus::badge()`
/// so the segments can be told apart at a glance.
fn status_segments(status: &GitStatus) -> Vec<(String, Color)> {
    let mut segments = Vec::new();
    if status.changed > 0 {
        segments.push((format!("~{}", status.changed), Color::Modified));
    }
    if status.untracked > 0 {
        segments.push((format!("?{}", status.untracked), Color::Created));
    }
    if status.ahead > 0 {
        segments.push((format!("\u{2191}{}", status.ahead), Color::Success));
    }
    if status.behind > 0 {
        segments.push((format!("\u{2193}{}", status.behind), Color::Info));
    }
    if !status.has_upstream {
        segments.push(("\u{26a0}".to_string(), Color::Warning));
    }
    segments
}

/// What a right-click offers on a row. Removing a worktree deletes its
/// directory, so it is withheld where the core would refuse anyway: the main
/// worktree, and any worktree holding work that git will not throw away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    RemoveWorktree,
    DeleteSession,
}

fn menu_action_for_row(
    kind: RowKind,
    is_main_root: bool,
    is_dirty: bool,
) -> Option<RowAction> {
    match kind {
        RowKind::Worktree if is_main_root || is_dirty => None,
        RowKind::Worktree => Some(RowAction::RemoveWorktree),
        RowKind::Session | RowKind::Orphan => Some(RowAction::DeleteSession),
        RowKind::Orphans | RowKind::More => None,
    }
}

fn agent_for_row(
    binding: Option<(&str, &str, SessionKind)>,
    active_agent: Option<&str>,
) -> Option<(String, Option<String>)> {
    match binding {
        // A live session is already running, and Claude Code files no
        // transcript while it is, so resuming it fails. If this panel started
        // it, its terminal is reused instead; if something else did, there is
        // nothing to attach to.
        Some((_, _, SessionKind::Live)) => None,
        Some((agent, session, _)) => Some((agent.to_string(), Some(session.to_string()))),
        None => active_agent.map(|agent| (agent.to_string(), None)),
    }
}

impl ZrushPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let repo = workspace.update_in(&mut cx, |workspace, _window, cx| {
            workspace
                .root_paths(cx)
                .into_iter()
                .next()
                .map(|path| path.as_ref().to_path_buf())
        })?;

        let host = ZedHost::default();
        let (service, error) = match repo.clone() {
            Some(repo) => match Self::service_for(repo, host.clone()) {
                Ok(service) => (Some(Arc::new(service)), None),
                Err(error) => (None, Some(error.to_string())),
            },
            None => (None, Some("Zrush needs a local project root".to_string())),
        };

        let panel = workspace.update_in(&mut cx, |_workspace, _window, cx| {
            let panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace.clone(),
                service,
                host,
                rows: Vec::new(),
                sessions: Vec::new(),
                statuses: HashMap::new(),
                collapsed: HashSet::new(),
                expanded: HashMap::new(),
                error,
                load_error: None,
                active: true,
                context_menu: None,
                refresh_task: None,
                auto_refresh: None,
            };
            let panel = cx.new(|_| panel);
            panel.update(cx, |panel, cx| {
                panel.refresh(cx);
                panel.start_auto_refresh(cx);
            });
            panel
        })?;

        // Deliberately outside the update above: starting the agent reads the
        // workspace, and reading an entity that is already being updated
        // panics.
        if let Some(root) = repo.as_deref() {
            let pending = cx.update(|_window, cx| PendingAgentRun::take_for(cx, root))?;
            if let Some((cwd, command)) = pending
                && let Some(workspace) = workspace.upgrade()
            {
                panel.update_in(&mut cx, |panel, window, cx| {
                    panel.spawn_agent_terminal(workspace, cwd, command, window, cx);
                })?;
            }
        }

        Ok(panel)
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

    /// Reloads the tree. The work shells out to git once per worktree and
    /// scans a transcript per agent, so it runs off the foreground thread.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        let collapsed = self.collapsed.clone();
        let expanded = self.expanded.clone();

        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { Self::load_rows(&service, &collapsed, &expanded) })
                .await;

            this.update(cx, |panel, cx| {
                match loaded {
                    Ok((rows, sessions, statuses)) => {
                        panel.rows = rows;
                        panel.sessions = sessions;
                        panel.statuses = statuses;
                        panel.load_error = None;
                    }
                    Err(err) => panel.load_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        self.auto_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS))
                    .await;
                let alive = this
                    .update(cx, |panel, cx| {
                        if panel.active {
                            panel.refresh(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    fn load_rows(
        service: &Zrush,
        collapsed: &HashSet<NodeId>,
        expanded: &HashMap<NodeId, usize>,
    ) -> Result<(Vec<Row>, Vec<Session>, HashMap<PathBuf, GitStatus>)> {
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

        Ok((rows, sessions, statuses))
    }

    fn spawn_agent_terminal(
        &mut self,
        workspace: Entity<Workspace>,
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

        let task_id = zrush_task_id(&cwd, &command);

        // Coming back to a running session must not re-run its command. Zed's
        // own task reuse goes through `replace_terminal`, which restarts the
        // agent, so find the terminal this session already owns and activate
        // it instead.
        let existing = workspace
            .read(cx)
            .items_of_type::<TerminalView>(cx)
            .find(|view| {
                view.read(cx)
                    .terminal()
                    .read(cx)
                    .task()
                    .is_some_and(|state| state.spawned_task.id == task_id)
            });
        if let Some(existing) = existing {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, true, window, cx);
            });
            return;
        }

        let label = format!("zrush · {}", program);
        let spawn = SpawnInTerminal {
            id: task_id,
            full_label: label.clone(),
            label,
            command: Some(program.clone()),
            args: args.to_vec(),
            command_label: command.join(" "),
            cwd: Some(cwd),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            show_summary: false,
            show_command: false,
            show_rerun: false,
            ..Default::default()
        };

        // `TerminalPanel::add_center_terminal` hands the item to the *active*
        // pane, which is not guaranteed to be a center one — a focused dock
        // pane swallows the terminal. Place it in the center ourselves.
        let project = workspace.read(cx).project().clone();
        let terminal = project.update(cx, |project, cx| project.create_terminal_task(spawn, cx));

        cx.spawn_in(window, async move |this, cx| {
            let terminal = match terminal.await {
                Ok(terminal) => terminal,
                Err(err) => {
                    return this.update(cx, |panel, cx| {
                        panel.error = Some(err.to_string());
                        cx.notify();
                    });
                }
            };

            let placed = workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_center(Box::new(view), window, cx)
            })?;

            if !placed {
                this.update(cx, |panel, cx| {
                    panel.error = Some("Zed has no center pane to open the agent terminal in".into());
                    cx.notify();
                })?;
            }

            Ok(())
        })
        .detach_and_log_err(cx);
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
        self.refresh(cx);
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
                .map(|session| (session.agent, session.id.clone(), session.kind))
        });
        let binding_ref = binding
            .as_ref()
            .map(|(agent, id, kind)| (*agent, id.as_str(), *kind));

        let open_binding = binding_ref.map(|(agent, id, _)| (agent, id));
        if let Err(err) = service.open(path, open_binding) {
            self.error = Some(err.to_string());
            cx.notify();
            return;
        }

        let choice = agent_for_row(binding_ref, service.active_agent().map(|agent| agent.id()));

        if let Some((agent, resume)) = choice.as_ref()
            && let Err(err) = service.run_agent(path, agent, resume.as_deref())
        {
            self.error = Some(err.to_string());
            cx.notify();
            return;
        }

        PendingAgentRun::clear(cx);

        let requests = match self.host.drain() {
            Ok(requests) => requests,
            Err(err) => {
                self.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };

        let mut switch_to = None;
        let mut agent_command = None;
        for request in requests {
            match request {
                HostRequest::OpenWorkspace(path) => switch_to = Some(path),
                HostRequest::RunAgent { cwd, command } => agent_command = Some((cwd, command)),
            }
        }

        let Some(workspace) = self.workspace.upgrade() else {
            self.error = Some("Zed workspace is no longer available".into());
            cx.notify();
            return;
        };

        // Only switch when the target worktree is not the one already open:
        // a switch tears the workspace down and would take the agent terminal
        // with it.
        let needs_switch = switch_to.as_ref().is_some_and(|target| {
            !workspace
                .read(cx)
                .root_paths(cx)
                .iter()
                .any(|root| root.as_ref() == target.as_path())
        });

        let Some(target) = switch_to.filter(|_| needs_switch) else {
            if let Some((cwd, command)) = agent_command {
                self.spawn_agent_terminal(workspace, cwd, command, window, cx);
            }
            return;
        };

        let display_name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.to_string_lossy().into_owned());

        // The terminal belongs to the workspace, and switching replaces it, so
        // the agent cannot be started here. Hand the request to the workspace
        // the switch is about to build: its panel picks it up in `load`.
        if let Some((cwd, command)) = agent_command {
            PendingAgentRun::set(cx, target.clone(), cwd, command);
        }

        window.dispatch_action(
            Box::new(SwitchWorktree {
                path: target,
                display_name,
            }),
            cx,
        );
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        row: Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let NodeId::Worktree(path) = row.node.clone() else {
            return;
        };
        let is_main_root = self
            .service
            .as_ref()
            .is_some_and(|service| service.main_root() == path);
        let is_dirty = self
            .statuses
            .get(&path)
            .is_some_and(|status| status.changed > 0 || status.untracked > 0);

        let Some(action) = menu_action_for_row(row.kind, is_main_root, is_dirty) else {
            return;
        };

        let panel = cx.entity().downgrade();
        let focus_handle = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            match action {
                RowAction::RemoveWorktree => menu.context(focus_handle).entry(
                    "Remove Worktree",
                    None,
                    move |window, cx| {
                        let path = path.clone();
                        panel
                            .update(cx, |panel, cx| {
                                panel.confirm_remove_worktree(path, window, cx);
                            })
                            .ok();
                    },
                ),
                RowAction::DeleteSession => {
                    let session_id = row.session_id.clone();
                    menu.context(focus_handle).entry(
                        "Delete Session",
                        None,
                        move |window, cx| {
                            let Some(id) = session_id.clone() else {
                                return;
                            };
                            let path = path.clone();
                            panel
                                .update(cx, |panel, cx| {
                                    panel.confirm_delete_session(id, path, window, cx);
                                })
                                .ok();
                        },
                    )
                }
            }
        });

        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |panel, _, _: &DismissEvent, cx| {
            panel.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn confirm_remove_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove the worktree at {}?", path.display()),
            Some("Its directory is deleted. Conversations recorded in it are kept."),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |panel, cx| {
                let Some(service) = panel.service.clone() else {
                    return;
                };
                // `with_sessions: false`: removing a worktree and deleting the
                // conversations held in it are two decisions, and this is only
                // the first.
                match service.remove_worktree(&path, &[], false) {
                    Ok(_) => panel.error = None,
                    Err(err) => panel.error = Some(err.to_string()),
                }
                panel.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete_session(
        &mut self,
        id: String,
        worktree: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let agent = session.agent;
        let running = session.kind == SessionKind::Live;

        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the conversation {id}?"),
            Some("Its transcript is removed for good."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |panel, cx| {
                let Some(service) = panel.service.clone() else {
                    return;
                };
                match service.delete_session(agent, &id, running, Some(&worktree)) {
                    Ok(_) => panel.error = None,
                    Err(err) => panel.error = Some(err.to_string()),
                }
                panel.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn render_row(
        &self,
        index: usize,
        row: Row,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let worktree_status = match &row.node {
            NodeId::Worktree(path) => self.statuses.get(path),
            NodeId::Orphans => None,
        };

        let clickable = row.kind == RowKind::More
            || (matches!(row.node, NodeId::Worktree(_))
                && !matches!(row.kind, RowKind::Orphan | RowKind::Orphans));

        let menu_row = row.clone();

        div()
            .id(("zrush-row", index))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |panel, event: &MouseDownEvent, window, cx| {
                    panel.deploy_context_menu(event.position, menu_row.clone(), window, cx);
                }),
            )
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
                        .when_some(worktree_status, |item, status| {
                            item.child(h_flex().gap_1().children(
                                status_segments(status).into_iter().map(|(text, color)| {
                                    Label::new(text).size(LabelSize::XSmall).color(color)
                                }),
                            ))
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
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(format!("{} rows", self.rows.len()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("zrush-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|panel, _, _window, cx| panel.refresh(cx))),
                    ),
            );

        let body = if let Some(error) = self.error.clone().or_else(|| self.load_error.clone()) {
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

        let context_menu = self.context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        });

        v_flex()
            .id("zrush-panel")
            .track_focus(&self.focus_handle(cx))
            .children(context_menu)
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

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match ZrushPanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .ok()
        else {
            return;
        };
        settings::update_settings_file(fs, cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.zrush_panel.get_or_insert_default().dock = Some(dock);
        });
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

#[cfg(test)]
mod tests {
    use super::{
        Color, GitStatus, PendingAgentRun, RowAction, RowKind, SessionKind, agent_for_row,
        menu_action_for_row, status_segments, zrush_task_id,
    };

    use gpui::TestAppContext;
    use std::path::{Path, PathBuf};

    fn pending(cx: &mut gpui::App, worktree: &str) {
        PendingAgentRun::set(
            cx,
            PathBuf::from(worktree),
            PathBuf::from(worktree),
            vec!["claude".to_string()],
        );
    }

    #[gpui::test]
    fn a_pending_run_is_refused_to_a_workspace_opened_for_another_worktree(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            pending(cx, "/repo/feature");
            assert!(PendingAgentRun::take_for(cx, Path::new("/repo/other")).is_none());
            assert!(PendingAgentRun::take_for(cx, Path::new("/repo/feature")).is_some());
        });
    }

    #[gpui::test]
    fn a_pending_run_fires_at_most_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            pending(cx, "/repo/feature");
            assert!(PendingAgentRun::take_for(cx, Path::new("/repo/feature")).is_some());
            assert!(PendingAgentRun::take_for(cx, Path::new("/repo/feature")).is_none());
        });
    }

    #[gpui::test]
    fn clearing_drops_a_run_left_behind_by_a_failed_switch(cx: &mut TestAppContext) {
        cx.update(|cx| {
            pending(cx, "/repo/feature");
            PendingAgentRun::clear(cx);
            assert!(PendingAgentRun::take_for(cx, Path::new("/repo/feature")).is_none());
        });
    }

    #[test]
    fn two_sessions_in_one_worktree_own_separate_terminals() {
        let cwd = Path::new("/repo/feature");
        let resume_a = vec!["claude".to_string(), "--resume".to_string(), "a".to_string()];
        let resume_b = vec!["claude".to_string(), "--resume".to_string(), "b".to_string()];
        assert_ne!(zrush_task_id(cwd, &resume_a), zrush_task_id(cwd, &resume_b));
    }

    #[test]
    fn the_same_session_keeps_the_same_terminal() {
        let cwd = Path::new("/repo/feature");
        let resume = vec!["claude".to_string(), "--resume".to_string(), "a".to_string()];
        assert_eq!(zrush_task_id(cwd, &resume), zrush_task_id(cwd, &resume));
    }

    #[test]
    fn the_same_command_in_two_worktrees_gets_two_terminals() {
        let start = vec!["claude".to_string()];
        assert_ne!(
            zrush_task_id(Path::new("/repo/main"), &start),
            zrush_task_id(Path::new("/repo/feature"), &start)
        );
    }

    #[test]
    fn a_clean_worktree_tracking_an_upstream_shows_nothing() {
        let clean = GitStatus {
            has_upstream: true,
            ..Default::default()
        };
        assert!(status_segments(&clean).is_empty());
    }

    #[test]
    fn each_kind_of_change_gets_its_own_colour() {
        let status = GitStatus {
            changed: 3,
            untracked: 1,
            ahead: 2,
            behind: 4,
            has_upstream: true,
        };
        assert_eq!(
            status_segments(&status),
            vec![
                ("~3".to_string(), Color::Modified),
                ("?1".to_string(), Color::Created),
                ("\u{2191}2".to_string(), Color::Success),
                ("\u{2193}4".to_string(), Color::Info),
            ]
        );
    }

    #[test]
    fn a_branch_without_an_upstream_is_flagged() {
        let orphan = GitStatus::default();
        let segments = status_segments(&orphan);
        assert_eq!(segments, vec![("\u{26a0}".to_string(), Color::Warning)]);
    }

    #[test]
    fn a_bound_session_resumes_its_own_agent() {
        assert_eq!(
            agent_for_row(Some(("claude", "session-1", SessionKind::Resumable)), Some("codex")),
            Some(("claude".to_string(), Some("session-1".to_string())))
        );
    }

    /// A live session is running somewhere already, and Claude Code files no
    /// transcript for it, so `--resume` fails with "no conversation found".
    #[test]
    fn a_dirty_worktree_is_not_offered_for_removal() {
        assert_eq!(
            menu_action_for_row(RowKind::Worktree, false, true),
            None,
            "git worktree remove refuses uncommitted work, so offering it would only fail"
        );
    }

    #[test]
    fn the_main_worktree_is_never_offered_for_removal() {
        assert_eq!(menu_action_for_row(RowKind::Worktree, true, false), None);
    }

    #[test]
    fn a_clean_side_worktree_can_be_removed() {
        assert_eq!(
            menu_action_for_row(RowKind::Worktree, false, false),
            Some(RowAction::RemoveWorktree)
        );
    }

    #[test]
    fn session_rows_offer_deletion_and_structural_rows_offer_nothing() {
        assert_eq!(
            menu_action_for_row(RowKind::Session, false, false),
            Some(RowAction::DeleteSession)
        );
        assert_eq!(
            menu_action_for_row(RowKind::Orphan, false, false),
            Some(RowAction::DeleteSession)
        );
        assert_eq!(menu_action_for_row(RowKind::Orphans, false, false), None);
        assert_eq!(menu_action_for_row(RowKind::More, false, false), None);
    }

    #[test]
    fn a_live_session_is_never_resumed() {
        assert_eq!(
            agent_for_row(Some(("claude", "session-1", SessionKind::Live)), Some("claude")),
            None
        );
    }

    #[test]
    fn an_unbound_row_starts_the_active_agent() {
        assert_eq!(
            agent_for_row(None, Some("claude")),
            Some(("claude".to_string(), None))
        );
    }

    #[test]
    fn an_unbound_row_without_an_active_agent_stays_a_plain_worktree_switch() {
        assert_eq!(agent_for_row(None, None), None);
    }
}
