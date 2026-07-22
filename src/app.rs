use std::path::PathBuf;

use relm4::adw;
use relm4::adw::prelude::{AdwDialogExt, AlertDialogExt};
use relm4::gtk;
use relm4::gtk::gdk::RGBA;
use relm4::gtk::gio;
use relm4::gtk::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt, SimpleComponent};
use vte4::{PtyFlags, Terminal, TerminalExt, TerminalExtManual};

use crate::state::{self, SavedGroup, SavedState, SavedTab};
use crate::tmuxctl::{self, SessionInfo, TmuxAvailability, TmuxCtl, TmuxError};

pub const GROUP_PALETTE: [&str; 6] = [
    "group-c0", "group-c1", "group-c2", "group-c3", "group-c4", "group-c5",
];

const SHORTCUTS: &[(&str, &str, Msg)] = &[
    ("<Control><Shift>t", "New tab in active group", Msg::NewTab),
    ("<Control><Shift>n", "New group", Msg::NewGroup),
    ("<Control><Shift>w", "Close active tab", Msg::CloseActive),
    (
        "<Control><Shift>m",
        "Move tab to another group",
        Msg::MoveTabPicker,
    ),
    ("<Control><Shift>g", "Jump to a group", Msg::JumpPicker),
    (
        "<Control><Shift>r",
        "Name the active group",
        Msg::RenameDialog,
    ),
    ("<Control>Page_Down", "Next tab", Msg::NavNext),
    ("<Control>Page_Up", "Previous tab", Msg::NavPrev),
    ("<Alt>Page_Down", "Next group", Msg::GroupNext),
    ("<Alt>Page_Up", "Previous group", Msg::GroupPrev),
    ("<Alt>1", "Toggle tab pane", Msg::ToggleSidebar),
    ("<Alt><Shift>1", "Toggle tab pane", Msg::ToggleSidebar),
    ("<Alt>exclam", "Toggle tab pane", Msg::ToggleSidebar),
    ("F1", "Show this help", Msg::ShowHelp),
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum PickerMode {
    Move,
    Jump,
}

pub struct Tab {
    id: usize,
    uuid: String, // stable across restarts; names the backing tmux session
    group: usize,
    title: String,
    crashed: Option<i32>, // shell exit code when the tab crashed
    terminal: Terminal,
}

pub struct Group {
    id: usize,
    name: String, // empty = unnamed, no header shown
    css: &'static str,
    last_active: usize,
}

pub struct App {
    tabs: Vec<Tab>,
    groups: Vec<Group>,
    active: Option<usize>,
    next_tab_id: usize,
    next_group_id: usize,
    sidebar_visible: bool,
    style: adw::StyleManager,
    tab_list: gtk::ListBox,
    tab_bar: gtk::Box,
    stack: gtk::Stack,
    window: gtk::ApplicationWindow,
    input: relm4::Sender<Msg>,
    /// tmux backing, present only when a usable tmux (>= 3.2) was found.
    tmux: Option<TmuxCtl>,
    /// Result of the one-time startup availability check (drives the warning).
    availability: TmuxAvailability,
    /// Watches the pane-died events dir; kept alive for its lifetime.
    monitor: Option<gio::FileMonitor>,
    /// Where the presentation model is persisted.
    state_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Msg {
    NewTab,
    NewGroup,
    Select(usize),
    CloseTab(usize),
    CloseActive,
    ChildExited(usize, i32),
    TitleChanged(usize, String),
    MoveTabPicker,
    MoveTabTo(Option<usize>), // None = new group
    JumpPicker,
    JumpToGroup(usize),
    DropTab {
        src: usize,
        dest: usize,
    },
    RenameDialog,
    RenameGroup(usize, String),
    NavNext,
    NavPrev,
    GroupNext,
    GroupPrev,
    ToggleSidebar,
    SetSidebar(bool),
    ShowHelp,
    SchemeChanged,
    /// The pane-died hook reported a crashed shell: (tab uuid, exit code).
    PaneCrashed(String, i32),
    /// Rerun the shell in a crashed tab (respawn-pane / fresh $SHELL).
    RestartTab(usize),
    /// Open the "tmux unavailable" explanation dialog.
    ShowTmuxWarning,
}

#[relm4::component(pub)]
impl SimpleComponent for App {
    type Init = ();
    type Input = Msg;
    type Output = ();

    view! {
        gtk::ApplicationWindow {
            set_title: Some("kabelsalat"),
            set_default_size: (1100, 700),

            #[wrap(Some)]
            set_titlebar = &gtk::HeaderBar {
                pack_start = &gtk::ToggleButton {
                    set_icon_name: "sidebar-show-symbolic",
                    set_tooltip_text: Some("Toggle tab pane (Alt+1)"),
                    #[watch]
                    set_active: model.sidebar_visible,
                    connect_toggled[sender] => move |button| {
                        sender.input(Msg::SetSidebar(button.is_active()));
                    },
                },

                pack_end = &gtk::Button {
                    set_icon_name: "help-about-symbolic",
                    set_tooltip_text: Some("Keyboard shortcuts (F1)"),
                    connect_clicked => Msg::ShowHelp,
                },

                pack_end = &gtk::Button {
                    set_icon_name: "dialog-warning-symbolic",
                    add_css_class: "tmux-warning",
                    set_tooltip_text: Some("Crash-safe sessions unavailable — click for details"),
                    #[watch]
                    set_visible: !matches!(model.availability, TmuxAvailability::Available(_)),
                    connect_clicked => Msg::ShowTmuxWarning,
                },
            },

            gtk::Paned {
                set_orientation: gtk::Orientation::Horizontal,
                set_position: 220,
                set_resize_start_child: false,
                set_shrink_start_child: false,

                #[wrap(Some)]
                set_start_child = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_width_request: 120,
                    #[watch]
                    set_visible: model.sidebar_visible,

                        gtk::Box {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_margin_all: 6,
                            set_spacing: 6,

                            gtk::Label {
                                set_label: "tabs",
                                set_hexpand: true,
                                set_halign: gtk::Align::Start,
                            },

                            gtk::Button {
                                set_icon_name: "folder-new-symbolic",
                                set_tooltip_text: Some("New group (Ctrl+Shift+N)"),
                                connect_clicked => Msg::NewGroup,
                            },

                            gtk::Button {
                                set_icon_name: "tab-new-symbolic",
                                set_tooltip_text: Some("New tab (Ctrl+Shift+T)"),
                                connect_clicked => Msg::NewTab,
                            },
                        },

                        #[local_ref]
                        tab_list -> gtk::ListBox {
                            set_vexpand: true,
                            add_css_class: "navigation-sidebar",
                            connect_row_selected[sender] => move |_, row| {
                                if let Some(id) = row.and_then(|r| r.widget_name().parse().ok()) {
                                    sender.input(Msg::Select(id));
                                }
                            },
                        },
                },

                #[wrap(Some)]
                set_end_child = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_hexpand: true,

                    append = &tab_bar.clone() {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 2,
                        set_margin_all: 4,
                        set_visible: false,
                    },

                    append = &stack.clone() {
                        set_hexpand: true,
                        set_vexpand: true,
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let style = adw::StyleManager::default();
        style.connect_dark_notify({
            let sender = sender.clone();
            move |_| {
                let _ = sender.input_sender().send(Msg::SchemeChanged);
            }
        });

        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);
        // Capture phase: the window sees keys before the focused terminal does,
        // so our triggers never reach the shell; all other keys pass through.
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        for (trigger, _, msg) in SHORTCUTS {
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::CallbackAction::new({
                    let sender = sender.clone();
                    let msg = msg.clone();
                    move |_, _| {
                        sender.input(msg.clone());
                        gtk::glib::Propagation::Stop
                    }
                })),
            ));
        }
        root.add_controller(controller);

        // Detect tmux once; only a usable (>= 3.2) tmux gets a live controller.
        let availability = tmuxctl::detect();
        let tmux = match &availability {
            TmuxAvailability::Available(_) => match TmuxCtl::new() {
                Ok(ctl) => Some(ctl),
                Err(err) => {
                    eprintln!("tmux setup failed, using direct shells: {err}");
                    None
                }
            },
            _ => None,
        };

        let mut model = App {
            tabs: Vec::new(),
            groups: Vec::new(),
            active: None,
            next_tab_id: 1,
            next_group_id: 1,
            sidebar_visible: true,
            style,
            tab_list: gtk::ListBox::new(),
            tab_bar: gtk::Box::new(gtk::Orientation::Horizontal, 2),
            stack: gtk::Stack::new(),
            window: root.clone(),
            input: sender.input_sender().clone(),
            tmux,
            availability,
            monitor: None,
            state_path: state::state_file(),
        };

        let tab_list = model.tab_list.clone();
        let tab_bar = model.tab_bar.clone();
        let stack = model.stack.clone();
        let widgets = view_output!();

        // Watch the pane-died events directory before reconciling so no crash
        // report is missed; clear stale files from previous runs first.
        if let Some(ctl) = &model.tmux {
            let events_dir = ctl.events_dir().to_path_buf();
            if let Ok(entries) = std::fs::read_dir(&events_dir) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let file = gio::File::for_path(&events_dir);
            match file.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
                Ok(mon) => {
                    let sender = sender.clone();
                    mon.connect_changed(move |_, file, _, event| {
                        if !matches!(
                            event,
                            gio::FileMonitorEvent::Created | gio::FileMonitorEvent::ChangesDoneHint
                        ) {
                            return;
                        }
                        let Some(path) = file.path() else { return };
                        let Ok(content) = std::fs::read_to_string(&path) else {
                            return;
                        };
                        if let Some((uuid, code)) = parse_pane_died_event(&content) {
                            let _ = sender.input_sender().send(Msg::PaneCrashed(uuid, code));
                        }
                        // One-shot: drop the file so it never re-fires.
                        let _ = std::fs::remove_file(&path);
                    });
                    model.monitor = Some(mon);
                }
                Err(err) => eprintln!("failed to watch tmux events dir: {err}"),
            }
        }

        model.restore_or_fresh(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            Msg::NewTab => {
                let group = self
                    .active_tab()
                    .map(|t| t.group)
                    .unwrap_or_else(|| self.groups[0].id);
                self.open_tab(group, &sender);
            }
            Msg::NewGroup => self.open_group(&sender),
            Msg::Select(id) => self.activate(id),
            Msg::CloseTab(id) => self.close_tab(id),
            Msg::CloseActive => {
                if let Some(id) = self.active {
                    self.close_tab(id);
                }
            }
            Msg::ChildExited(id, status) => {
                if let Some(tmux) = &self.tmux {
                    // The child VTE saw is the tmux *client*, not the shell. If
                    // the session still lives (external detach), reattach;
                    // otherwise the session is gone (clean exit) → close.
                    let uuid = self
                        .tabs
                        .iter()
                        .find(|t| t.id == id)
                        .map(|t| t.uuid.clone());
                    if let Some(uuid) = uuid {
                        // Only a *definitive* empty result proves the session
                        // is gone. A query error (transient fork failure,
                        // busy server, novel stderr) means liveness is unknown
                        // — treat it conservatively as still alive and reattach
                        // (`new-session -A` is idempotent), because close_tab
                        // would kill a possibly-running shell and lose it.
                        let gone = session_definitively_gone(&tmux.list_sessions(), &uuid);
                        if gone {
                            self.close_tab(id);
                        } else if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                            spawn_backing(&tab.terminal, &uuid, Some(tmux));
                        }
                    }
                } else if gtk::glib::spawn_check_wait_status(status).is_ok() {
                    // No tmux: `status` is the shell's raw waitpid status.
                    self.close_tab(id);
                } else if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.crashed = Some(decode_exit(status));
                    self.rebuild_list();
                }
            }
            Msg::PaneCrashed(uuid, code) => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.uuid == uuid) {
                    tab.crashed = Some(code);
                    self.rebuild_list();
                }
            }
            Msg::RestartTab(id) => self.restart_tab(id),
            Msg::ShowTmuxWarning => self.show_tmux_warning(),
            Msg::TitleChanged(id, title) => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.title = title;
                    self.rebuild_list();
                }
            }
            Msg::MoveTabPicker => self.show_group_picker(PickerMode::Move),
            Msg::MoveTabTo(target) => self.move_active_tab(target),
            Msg::JumpPicker => self.show_group_picker(PickerMode::Jump),
            Msg::JumpToGroup(id) => {
                if let Some(tab) = self.tabs.iter().find(|t| t.group == id) {
                    self.activate(tab.id);
                }
            }
            Msg::DropTab { src, dest } => self.drop_tab(src, dest),
            Msg::RenameDialog => self.show_rename_dialog(),
            Msg::RenameGroup(id, name) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.name = name.trim().to_string();
                    self.rebuild_list();
                }
            }
            Msg::NavNext => self.navigate(1),
            Msg::NavPrev => self.navigate(-1),
            Msg::GroupNext => self.navigate_group(1),
            Msg::GroupPrev => self.navigate_group(-1),
            Msg::ToggleSidebar => self.set_sidebar(!self.sidebar_visible),
            Msg::SetSidebar(visible) => self.set_sidebar(visible),
            Msg::ShowHelp => self.show_help(),
            Msg::SchemeChanged => {
                for tab in &self.tabs {
                    apply_scheme(&tab.terminal, self.style.is_dark());
                }
            }
        }
        // Every layout mutation persists; writes are atomic and human-paced.
        self.save_state();
    }
}

impl App {
    fn active_tab(&self) -> Option<&Tab> {
        self.active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
    }

    fn active_group(&self) -> Option<usize> {
        self.active_tab().map(|t| t.group)
    }

    /// Tab ids in display order: groups in creation order, tabs within them.
    fn nav_order(&self) -> Vec<usize> {
        self.groups
            .iter()
            .flat_map(|g| self.tabs.iter().filter(|t| t.group == g.id).map(|t| t.id))
            .collect()
    }

    fn create_group(&mut self) -> usize {
        let id = self.next_group_id;
        self.next_group_id += 1;
        self.groups.push(Group {
            id,
            name: String::new(),
            css: GROUP_PALETTE[(id - 1) % GROUP_PALETTE.len()],
            last_active: 0,
        });
        id
    }

    fn open_group(&mut self, sender: &ComponentSender<Self>) {
        let id = self.create_group();
        self.open_tab(id, sender);
    }

    /// Startup reconciliation: restore the saved layout against live tmux
    /// sessions, or fall back to a fresh single tab. Persists the result.
    fn restore_or_fresh(&mut self, sender: &ComponentSender<Self>) {
        let saved = state::load(&self.state_path);
        self.sidebar_visible = saved.sidebar_visible;

        // Live sessions (and which panes are dead) from our private server.
        let (live, dead): (Vec<String>, Vec<state::DeadPane>) = match &self.tmux {
            Some(ctl) => match ctl.list_sessions() {
                Ok(sessions) => (
                    sessions.iter().map(|s| s.uuid.clone()).collect(),
                    sessions
                        .iter()
                        .filter(|s| s.pane_dead)
                        .map(|s| state::DeadPane {
                            uuid: s.uuid.clone(),
                            exit_code: s.dead_status.unwrap_or(-1),
                        })
                        .collect(),
                ),
                Err(err) => {
                    eprintln!("tmux list-sessions failed: {err}");
                    (Vec::new(), Vec::new())
                }
            },
            None => (Vec::new(), Vec::new()),
        };

        let plan = state::reconcile(&saved, &live, &dead);
        if plan.attach.is_empty() && plan.respawn.is_empty() && plan.adopt.is_empty() {
            // Empty state and no sessions → current fresh-start behavior.
            self.sidebar_visible = true;
            self.open_group(sender);
            self.save_state();
            return;
        }

        // Recreate saved groups; keep the id allocator above every restored id.
        for group in &saved.groups {
            self.groups.push(Group {
                id: group.id,
                name: group.name.clone(),
                css: GROUP_PALETTE[group.palette % GROUP_PALETTE.len()],
                last_active: 0,
            });
        }
        self.next_group_id = saved.groups.iter().map(|g| g.id).max().map_or(1, |m| m + 1);

        // Saved tabs, in order; attach live ones (crashed if the pane died),
        // respawn the rest. `spawn_argv -A` attaches or creates uniformly.
        let dead_exit = |uuid: &str| dead.iter().find(|d| d.uuid == uuid).map(|d| d.exit_code);
        for tab in &saved.tabs {
            self.add_tab(
                tab.uuid.clone(),
                tab.group,
                Some(tab.title.clone()),
                dead_exit(&tab.uuid),
                sender,
            );
        }

        // Orphan live sessions with no saved tab → a "Recovered" group so no
        // live shell is ever invisible.
        if !plan.adopt.is_empty() {
            let gid = self.create_group();
            if let Some(group) = self.groups.iter_mut().find(|g| g.id == gid) {
                group.name = "Recovered".to_string();
            }
            for orphan in &plan.adopt {
                let title = format!("Recovered {}", &orphan.uuid[..orphan.uuid.len().min(8)]);
                self.add_tab(
                    orphan.uuid.clone(),
                    gid,
                    Some(title),
                    orphan.dead_exit,
                    sender,
                );
            }
        }

        // Drop any group that ended up empty, then restore the active tab.
        self.groups
            .retain(|g| self.tabs.iter().any(|t| t.group == g.id));
        let active_id = saved
            .active
            .as_ref()
            .and_then(|u| self.tabs.iter().find(|t| t.uuid == *u).map(|t| t.id))
            .or_else(|| self.tabs.first().map(|t| t.id));
        if let Some(id) = active_id {
            self.activate(id);
        }
        self.save_state();
    }

    /// Serialize the current presentation model to disk (atomic write).
    fn save_state(&self) {
        let groups = self
            .groups
            .iter()
            .map(|g| SavedGroup {
                id: g.id,
                name: g.name.clone(),
                palette: GROUP_PALETTE.iter().position(|c| *c == g.css).unwrap_or(0),
            })
            .collect();
        let tabs = self
            .tabs
            .iter()
            .map(|t| SavedTab {
                uuid: t.uuid.clone(),
                group: t.group,
                title: t.title.clone(),
            })
            .collect();
        let active = self
            .active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
            .map(|t| t.uuid.clone());
        let state = SavedState {
            groups,
            tabs,
            active,
            sidebar_visible: self.sidebar_visible,
        };
        if let Err(err) = state::save(&state, &self.state_path) {
            eprintln!("failed to save state: {err}");
        }
    }

    /// Explain that crash-safe sessions need tmux >= 3.2, with install hints.
    fn show_tmux_warning(&self) {
        let body = tmux_warning_body(&self.availability);
        let dialog = adw::AlertDialog::new(Some("Crash-safe sessions unavailable"), Some(&body));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }

    fn move_active_tab(&mut self, target: Option<usize>) {
        let Some(active) = self.active else { return };
        let target = match target {
            Some(id) if self.groups.iter().any(|g| g.id == id) => id,
            Some(_) => return, // group vanished while the picker was open
            None => self.create_group(),
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active) else {
            return;
        };
        if tab.group == target {
            return;
        }
        tab.group = target;
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == target) {
            group.last_active = active;
        }
        self.groups
            .retain(|g| self.tabs.iter().any(|t| t.group == g.id));
        self.rebuild_list();
    }

    /// Modal group picker: arrow keys + Enter, or a single click. Esc closes
    /// (built into adw::Dialog). Move mode offers a "New group" target;
    /// jump mode activates the chosen group's last-active tab.
    fn show_group_picker(&self, mode: PickerMode) {
        let Some(current_group) = self.active_group() else {
            return;
        };

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");

        let mut first_row: Option<gtk::ListBoxRow> = None;
        for group in self.groups.iter().filter(|g| g.id != current_group) {
            let members: Vec<&Tab> = self.tabs.iter().filter(|t| t.group == group.id).collect();
            let representative = members
                .iter()
                .find(|t| t.id == group.last_active)
                .unwrap_or(&members[0]);
            let label = gtk::Label::builder()
                .label(format!("{} ({})", representative.title, members.len()))
                .halign(gtk::Align::Start)
                .margin_start(6)
                .build();
            let row = gtk::ListBoxRow::builder().child(&label).build();
            row.set_widget_name(&group.id.to_string());
            row.add_css_class(group.css);
            list.append(&row);
            first_row.get_or_insert(row);
        }
        if mode == PickerMode::Move {
            let new_label = gtk::Label::builder()
                .label("New group")
                .halign(gtk::Align::Start)
                .margin_start(6)
                .build();
            let new_row = gtk::ListBoxRow::builder().child(&new_label).build();
            new_row.set_widget_name("new");
            list.append(&new_row);
            first_row.get_or_insert(new_row);
        }
        let Some(first_row) = first_row else { return }; // nothing to pick

        let dialog = adw::Dialog::builder()
            .title(match mode {
                PickerMode::Move => "Move tab to group",
                PickerMode::Jump => "Jump to group",
            })
            .content_width(320)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&adw::HeaderBar::new());
        content.append(&list);
        dialog.set_child(Some(&content));

        list.connect_row_activated({
            let input = self.input.clone();
            let dialog = dialog.clone();
            move |_, row| {
                let name = row.widget_name();
                match (mode, name.as_str(), name.parse().ok()) {
                    (PickerMode::Move, "new", _) => {
                        let _ = input.send(Msg::MoveTabTo(None));
                    }
                    (PickerMode::Move, _, Some(id)) => {
                        let _ = input.send(Msg::MoveTabTo(Some(id)));
                    }
                    (PickerMode::Jump, _, Some(id)) => {
                        let _ = input.send(Msg::JumpToGroup(id));
                    }
                    _ => {}
                }
                dialog.close();
            }
        });

        dialog.present(Some(&self.window));
        list.select_row(Some(&first_row));
        first_row.grab_focus();
    }

    /// Fresh user-initiated tab: new UUID, backing session spawned, activated.
    fn open_tab(&mut self, group: usize, sender: &ComponentSender<Self>) {
        let uuid = gtk::glib::uuid_string_random().to_string();
        let id = self.add_tab(uuid, group, None, None, sender);
        self.activate(id);
    }

    /// Create a tab backed by `uuid` (spawning its tmux session, or a direct
    /// $SHELL in the fallback path) without changing the active tab. Returns
    /// the new tab id. `title = None` uses the default "Terminal N".
    fn add_tab(
        &mut self,
        uuid: String,
        group: usize,
        title: Option<String>,
        crashed: Option<i32>,
        sender: &ComponentSender<Self>,
    ) -> usize {
        let terminal = Terminal::builder().hexpand(true).vexpand(true).build();
        apply_scheme(&terminal, self.style.is_dark());
        spawn_backing(&terminal, &uuid, self.tmux.as_ref());

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let title = title.unwrap_or_else(|| format!("Terminal {id}"));

        #[allow(deprecated)] // successor termprop API needs VTE >= 0.78 feature gates
        terminal.connect_window_title_notify({
            let sender = sender.clone();
            move |terminal| {
                if let Some(title) = terminal.window_title().filter(|t| !t.is_empty()) {
                    let _ = sender
                        .input_sender()
                        .send(Msg::TitleChanged(id, title.to_string()));
                }
            }
        });

        terminal.connect_child_exited({
            let sender = sender.clone();
            // input_sender: child-exited can fire during teardown, after the runtime is gone
            move |_, status| {
                let _ = sender.input_sender().send(Msg::ChildExited(id, status));
            }
        });

        self.stack.add_child(&terminal);
        self.tabs.push(Tab {
            id,
            uuid,
            group,
            title,
            crashed,
            terminal,
        });
        id
    }

    /// Rerun the shell in a crashed tab, clearing its crashed marker. With
    /// tmux the dead pane is respawned in place (client stays attached); in
    /// the fallback path a fresh $SHELL is spawned into the same terminal.
    fn restart_tab(&mut self, id: usize) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let uuid = tab.uuid.clone();
        let terminal = tab.terminal.clone();
        match &self.tmux {
            Some(tmux) => {
                if let Err(err) = tmux.respawn_pane(&uuid) {
                    eprintln!("failed to respawn pane: {err}");
                    return;
                }
            }
            None => spawn_shell(&terminal),
        }
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.crashed = None;
        }
        self.rebuild_list();
    }

    fn activate(&mut self, id: usize) {
        if self.active == Some(id) {
            return; // also breaks the select_row → row-selected → Select cycle
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        self.active = Some(id);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == tab.group) {
            group.last_active = id;
        }
        self.stack.set_visible_child(&tab.terminal);
        tab.terminal.grab_focus();
        self.rebuild_list();
    }

    fn close_tab(&mut self, id: usize) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let order = self.nav_order();
        let tab = self.tabs.remove(index);
        // Explicit close is the only thing that kills the backing session.
        if let Some(tmux) = &self.tmux
            && let Err(err) = tmux.kill_session(&tab.uuid)
        {
            eprintln!("failed to kill session {}: {err}", tab.uuid);
        }
        self.stack.remove(&tab.terminal);
        self.groups
            .retain(|g| self.tabs.iter().any(|t| t.group == g.id));

        if self.tabs.is_empty() {
            relm4::main_application().quit();
            return;
        }
        if self.active == Some(id) {
            self.active = None;
            let pos = order.iter().position(|&t| t == id).unwrap_or(0);
            let next = order
                .iter()
                .cycle()
                .skip(pos + 1)
                .find(|&&t| self.tabs.iter().any(|tab| tab.id == t))
                .copied();
            if let Some(next) = next {
                self.activate(next);
                return;
            }
        }
        self.rebuild_list();
    }

    fn navigate(&mut self, step: isize) {
        let order = self.nav_order();
        let Some(active) = self.active else { return };
        let Some(pos) = order.iter().position(|&t| t == active) else {
            return;
        };
        let next = (pos as isize + step).rem_euclid(order.len() as isize) as usize;
        self.activate(order[next]);
    }

    /// Reorder by drag-and-drop: insert `src` before `dest`. If `dest` is in
    /// another group (e.g. a collapsed group's row), the tab moves there too —
    /// within-group order is just the tabs vec order, filtered per group.
    fn drop_tab(&mut self, src: usize, dest: usize) {
        if src == dest {
            return;
        }
        let Some(si) = self.tabs.iter().position(|t| t.id == src) else {
            return;
        };
        let mut tab = self.tabs.remove(si);
        let Some(di) = self.tabs.iter().position(|t| t.id == dest) else {
            self.tabs.insert(si, tab);
            return;
        };
        tab.group = self.tabs[di].group;
        let moved_group = tab.group;
        self.tabs.insert(di, tab);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == moved_group)
            && self.active == Some(src)
        {
            group.last_active = src;
        }
        self.groups
            .retain(|g| self.tabs.iter().any(|t| t.group == g.id));
        self.rebuild_list();
    }

    /// Jump to the first tab of the next/previous group, wrapping around.
    fn navigate_group(&mut self, step: isize) {
        let Some(current) = self.active_group() else {
            return;
        };
        let Some(pos) = self.groups.iter().position(|g| g.id == current) else {
            return;
        };
        let next = (pos as isize + step).rem_euclid(self.groups.len() as isize) as usize;
        let target = self.groups[next].id;
        if let Some(first) = self.tabs.iter().find(|t| t.group == target) {
            self.activate(first.id);
        }
    }

    /// Re-render the sidebar from model state. The active tab's group is shown
    /// expanded (one row per tab); every other group collapses to a single row
    /// showing its last-active tab plus the group tab count.
    fn rebuild_list(&self) {
        while let Some(row) = self.tab_list.row_at_index(0) {
            self.tab_list.remove(&row);
        }

        let active_group = self.active_group();
        for group in &self.groups {
            let members: Vec<&Tab> = self.tabs.iter().filter(|t| t.group == group.id).collect();
            if members.is_empty() {
                continue;
            }
            if !group.name.is_empty() {
                let label = gtk::Label::builder()
                    .label(&group.name)
                    .halign(gtk::Align::Start)
                    .build();
                let header = gtk::ListBoxRow::builder()
                    .child(&label)
                    .selectable(false)
                    .activatable(false)
                    .build();
                header.add_css_class("group-header");
                self.tab_list.append(&header);
            }
            if Some(group.id) == active_group {
                for tab in &members {
                    self.tab_list.append(&self.make_row(tab, group, None));
                }
            } else {
                let representative = members
                    .iter()
                    .find(|t| t.id == group.last_active)
                    .unwrap_or(&members[0]);
                self.tab_list
                    .append(&self.make_row(representative, group, Some(members.len())));
            }
        }

        self.rebuild_tab_bar();

        // Re-select the active row; activate()'s no-op guard stops the echo.
        if let Some(active) = self.active {
            let mut i = 0;
            while let Some(row) = self.tab_list.row_at_index(i) {
                if row.widget_name() == active.to_string() {
                    self.tab_list.select_row(Some(&row));
                    break;
                }
                i += 1;
            }
        }
    }

    /// Horizontal tab strip above the terminal, mirroring the active group.
    /// Only shown when that group has more than one tab; independent of the
    /// sidebar's visibility.
    fn rebuild_tab_bar(&self) {
        while let Some(child) = self.tab_bar.first_child() {
            self.tab_bar.remove(&child);
        }
        let members: Vec<&Tab> = match self.active_group() {
            Some(group) => self.tabs.iter().filter(|t| t.group == group).collect(),
            None => Vec::new(),
        };
        self.tab_bar.set_visible(members.len() > 1);
        if members.len() <= 1 {
            return;
        }
        let group_css = self
            .active_tab()
            .and_then(|t| self.groups.iter().find(|g| g.id == t.group))
            .map(|g| g.css);
        for tab in members {
            let button = gtk::Button::builder().label(&tab.title).build();
            button.add_css_class("flat");
            if let Some(css) = group_css {
                button.add_css_class(css);
            }
            if self.active == Some(tab.id) {
                button.add_css_class("tab-active");
            }
            if tab.crashed.is_some() {
                button.add_css_class("tab-crashed");
            }
            let id = tab.id;
            let input = self.input.clone();
            button.connect_clicked(move |_| {
                let _ = input.send(Msg::Select(id));
            });
            self.tab_bar.append(&button);
        }
    }

    fn make_row(
        &self,
        tab: &Tab,
        group: &Group,
        collapsed_count: Option<usize>,
    ) -> gtk::ListBoxRow {
        let label = match collapsed_count {
            Some(n) => format!("{} ({n})", tab.title),
            None => crashed_tab_label(&tab.title, tab.crashed),
        };
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        row_box.append(
            &gtk::Label::builder()
                .label(&label)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        if collapsed_count.is_none() && tab.crashed.is_some() {
            let restart = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Restart shell")
                .build();
            restart.add_css_class("flat");
            restart.add_css_class("circular");
            row_box.append(&restart);
            let id = tab.id;
            let input = self.input.clone();
            restart.connect_clicked(move |_| {
                let _ = input.send(Msg::RestartTab(id));
            });
        }
        if collapsed_count.is_none() {
            let close = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Close tab")
                .build();
            close.add_css_class("flat");
            close.add_css_class("circular");
            row_box.append(&close);
            let id = tab.id;
            let input = self.input.clone();
            close.connect_clicked(move |_| {
                let _ = input.send(Msg::CloseTab(id));
            });
        }

        let row = gtk::ListBoxRow::builder().child(&row_box).build();
        row.set_widget_name(&tab.id.to_string());
        row.add_css_class(group.css);
        if tab.crashed.is_some() {
            row.add_css_class("tab-crashed");
        }

        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        let src_id = tab.id;
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &src_id.to_string().to_value(),
            ))
        });
        row.add_controller(drag);

        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let dest_id = tab.id;
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(src) = value.get::<String>() else {
                return false;
            };
            let Ok(src) = src.parse() else { return false };
            let _ = input.send(Msg::DropTab { src, dest: dest_id });
            true
        });
        row.add_controller(drop);

        row
    }

    /// Name (or un-name) the active group via a small entry dialog.
    /// Enter applies, Esc cancels, an empty name removes the header.
    fn show_rename_dialog(&self) {
        let Some(group) = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
        else {
            return;
        };

        let entry = gtk::Entry::builder()
            .text(&group.name)
            .placeholder_text("Group name (empty to clear)")
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some("Name Group"), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("apply", "Apply");
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");

        let id = group.id;
        let input = self.input.clone();
        dialog.connect_response(Some("apply"), move |_, _| {
            let _ = input.send(Msg::RenameGroup(id, entry.text().to_string()));
        });
        dialog.present(Some(&self.window));
    }

    fn set_sidebar(&mut self, visible: bool) {
        if self.sidebar_visible == visible {
            return;
        }
        self.sidebar_visible = visible;
        if !visible && let Some(tab) = self.active_tab() {
            tab.terminal.grab_focus();
        }
    }

    fn show_help(&self) {
        let body: String = SHORTCUTS
            .iter()
            .filter(|(t, _, _)| *t != "<Alt><Shift>1" && *t != "<Alt>exclam")
            .map(|(trigger, desc, _)| {
                format!(
                    "{}  —  {desc}\n",
                    trigger
                        .replace("<Control>", "Ctrl+")
                        .replace("<Shift>", "Shift+")
                        .replace("<Alt>", "Alt+")
                        .replace("Page_Down", "PgDn")
                        .replace("Page_Up", "PgUp")
                )
            })
            .collect();
        let dialog = adw::AlertDialog::new(Some("Keyboard Shortcuts"), Some(body.trim_end()));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }
}

/// Whether a `list-sessions` result *definitively* proves the tab's backing
/// session is gone. Only a successful query lacking the uuid counts as gone;
/// any error means liveness is unknown, so we must treat it as still alive
/// (finding 1) rather than kill a possibly-running shell.
fn session_definitively_gone(result: &Result<Vec<SessionInfo>, TmuxError>, uuid: &str) -> bool {
    matches!(result, Ok(sessions) if !sessions.iter().any(|si| si.uuid == uuid))
}

/// Sidebar/tab-bar label for a (possibly crashed) tab. A negative exit code is
/// the "unknown exit" sentinel (a dead pane whose status tmux never recorded);
/// since no shell returns a negative code, render "?" rather than a fake -1
/// (finding 2).
fn crashed_tab_label(title: &str, crashed: Option<i32>) -> String {
    match crashed {
        Some(code) if code < 0 => format!("{title} [exit ?]"),
        Some(code) => format!("{title} [exit {code}]"),
        None => title.to_string(),
    }
}

/// Parse a pane-died event file's contents ("ks-<uuid> <code>") into
/// (uuid, exit code). Any deviation — empty read (the double-fire / race),
/// a missing/non-numeric code, or a name without the "ks-" prefix — yields
/// None so the handler is a harmless no-op (finding 5).
fn parse_pane_died_event(content: &str) -> Option<(String, i32)> {
    let mut parts = content.split_whitespace();
    let name = parts.next()?;
    let code = parts.next()?;
    let uuid = name.strip_prefix("ks-")?;
    let code = code.parse::<i32>().ok()?;
    Some((uuid.to_string(), code))
}

/// Body text of the "tmux unavailable" warning dialog. The TooOld branch names
/// the detected version and the required minimum in the lead; install hints
/// (apt/dnf/zypper) appear in every branch (finding 4).
fn tmux_warning_body(availability: &TmuxAvailability) -> String {
    let (major, minor) = tmuxctl::MIN_VERSION;
    let lead = match availability {
        TmuxAvailability::TooOld(found) => format!(
            "kabelsalat found tmux {found}, but crash-safe sessions require \
             tmux {major}.{minor} or newer. Please update tmux."
        ),
        _ => format!(
            "Installing tmux {major}.{minor} or newer lets your shells keep \
             running even if kabelsalat is closed, crashes, or is upgraded."
        ),
    };
    format!(
        "{lead}\n\nInstall hints:\n\
         \u{2022} Debian/Ubuntu:  apt install tmux\n\
         \u{2022} Fedora/Red Hat: dnf install tmux\n\
         \u{2022} openSUSE/SLE:   zypper install tmux"
    )
}

fn apply_scheme(terminal: &Terminal, dark: bool) {
    let (fg, bg) = if dark {
        ("#deddda", "#1d1d20")
    } else {
        ("#1d1d20", "#ffffff")
    };
    let fg = RGBA::parse(fg).unwrap();
    let bg = RGBA::parse(bg).unwrap();
    terminal.set_colors(Some(&fg), Some(&bg), &[]);
}

/// Decode a raw waitpid-style status into a user-facing exit code. Used only
/// in the no-tmux fallback, where the shell's own status reaches `ChildExited`.
fn decode_exit(status: i32) -> i32 {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff // WIFEXITED: WEXITSTATUS
    } else {
        128 + (status & 0x7f) // killed by signal: conventional 128+signo
    }
}

/// Spawn a tab's backing process: the tmux client for its session when tmux is
/// available (`new-session -A` attaches or creates), else a direct $SHELL.
fn spawn_backing(terminal: &Terminal, uuid: &str, tmux: Option<&TmuxCtl>) {
    let Some(ctl) = tmux else {
        spawn_shell(terminal);
        return;
    };
    let argv = ctl.spawn_argv(uuid);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        None,
        &refs,
        &[],
        gtk::glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                eprintln!("failed to attach tmux session: {err}");
            }
        },
    );
}

fn spawn_shell(terminal: &Terminal) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        None,
        &[&shell],
        &[],
        gtk::glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                eprintln!("failed to spawn shell: {err}");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(uuid: &str) -> SessionInfo {
        SessionInfo {
            uuid: uuid.to_string(),
            pane_dead: false,
            dead_status: None,
        }
    }

    // --- finding 1: session-gone decision -------------------------------

    #[test]
    fn gone_query_error_is_not_gone() {
        // A transient list-sessions error must NEVER route to kill/close.
        let err: Result<Vec<SessionInfo>, TmuxError> =
            Err(TmuxError::Command("busy server".into()));
        assert!(!session_definitively_gone(&err, "abc"));
    }

    #[test]
    fn gone_ok_without_uuid_is_gone() {
        let ok = Ok(vec![session("other")]);
        assert!(session_definitively_gone(&ok, "abc"));
    }

    #[test]
    fn gone_ok_empty_list_is_gone() {
        let ok: Result<Vec<SessionInfo>, TmuxError> = Ok(Vec::new());
        assert!(session_definitively_gone(&ok, "abc"));
    }

    #[test]
    fn gone_ok_with_uuid_is_alive() {
        let ok = Ok(vec![session("other"), session("abc")]);
        assert!(!session_definitively_gone(&ok, "abc"));
    }

    // --- finding 2: crashed-tab label -----------------------------------

    #[test]
    fn label_real_exit_code() {
        assert_eq!(crashed_tab_label("bash", Some(3)), "bash [exit 3]");
    }

    #[test]
    fn label_negative_code_is_unknown_sentinel() {
        // Never render a fake -1; a negative code means "unknown exit".
        assert_eq!(crashed_tab_label("bash", Some(-1)), "bash [exit ?]");
    }

    #[test]
    fn label_not_crashed_is_plain_title() {
        assert_eq!(crashed_tab_label("bash", None), "bash");
    }

    // --- finding 5: pane-died event-file parsing ------------------------

    #[test]
    fn parse_event_valid() {
        assert_eq!(
            parse_pane_died_event("ks-1234-abcd 137"),
            Some(("1234-abcd".to_string(), 137))
        );
    }

    #[test]
    fn parse_event_empty_is_none() {
        // The race the verifier analyzed: an empty read must be a no-op.
        assert_eq!(parse_pane_died_event(""), None);
        assert_eq!(parse_pane_died_event("   \n"), None);
    }

    #[test]
    fn parse_event_malformed_code_is_none() {
        assert_eq!(parse_pane_died_event("ks-abc notanumber"), None);
        assert_eq!(parse_pane_died_event("ks-abc"), None); // no code field
    }

    #[test]
    fn parse_event_missing_prefix_is_none() {
        assert_eq!(parse_pane_died_event("abc 1"), None);
    }

    // --- finding 4: warning-dialog body builder -------------------------

    #[test]
    fn warning_missing_has_install_hints_and_minimum() {
        let body = tmux_warning_body(&TmuxAvailability::Missing);
        assert!(body.contains("apt install tmux"));
        assert!(body.contains("dnf install tmux"));
        assert!(body.contains("zypper install tmux"));
        assert!(body.contains("3.2"));
    }

    #[test]
    fn warning_too_old_names_detected_and_required_versions() {
        let found = crate::tmuxctl::TmuxVersion::parse("tmux 3.1c").unwrap();
        let body = tmux_warning_body(&TmuxAvailability::TooOld(found));
        assert!(body.contains("3.1c")); // detected version
        assert!(body.contains("3.2")); // required minimum
        assert!(body.contains("apt install tmux"));
    }
}
