use relm4::adw;
use relm4::adw::prelude::{AdwDialogExt, AlertDialogExt};
use relm4::gtk;
use relm4::gtk::gdk::RGBA;
use relm4::gtk::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt, SimpleComponent};
use vte4::{PtyFlags, Terminal, TerminalExt, TerminalExtManual};

pub const GROUP_PALETTE: [&str; 6] = [
    "group-c0", "group-c1", "group-c2", "group-c3", "group-c4", "group-c5",
];

const SHORTCUTS: &[(&str, &str, Msg)] = &[
    ("<Control><Shift>t", "New tab in active group", Msg::NewTab),
    ("<Control><Shift>n", "New group", Msg::NewGroup),
    ("<Control><Shift>w", "Close active tab", Msg::CloseActive),
    ("<Control><Shift>m", "Move tab to another group", Msg::MoveTabPicker),
    ("<Control><Shift>g", "Jump to a group", Msg::JumpPicker),
    ("<Control><Shift>r", "Name the active group", Msg::RenameDialog),
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
    group: usize,
    title: String,
    crashed: bool,
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
    DropTab { src: usize, dest: usize },
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
            move |_| sender.input(Msg::SchemeChanged)
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
        };

        let tab_list = model.tab_list.clone();
        let tab_bar = model.tab_bar.clone();
        let stack = model.stack.clone();
        let widgets = view_output!();

        model.open_group(&sender);
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
                // `status` is a raw waitpid-style status; glib decodes it for us.
                if gtk::glib::spawn_check_wait_status(status).is_ok() {
                    self.close_tab(id);
                } else if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.crashed = true;
                    self.rebuild_list();
                }
            }
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
    }
}

impl App {
    fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tabs.iter().find(|t| t.id == id))
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

    fn move_active_tab(&mut self, target: Option<usize>) {
        let Some(active) = self.active else { return };
        let target = match target {
            Some(id) if self.groups.iter().any(|g| g.id == id) => id,
            Some(_) => return, // group vanished while the picker was open
            None => self.create_group(),
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active) else { return };
        if tab.group == target {
            return;
        }
        tab.group = target;
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == target) {
            group.last_active = active;
        }
        self.groups.retain(|g| self.tabs.iter().any(|t| t.group == g.id));
        self.rebuild_list();
    }

    /// Modal group picker: arrow keys + Enter, or a single click. Esc closes
    /// (built into adw::Dialog). Move mode offers a "New group" target;
    /// jump mode activates the chosen group's last-active tab.
    fn show_group_picker(&self, mode: PickerMode) {
        let Some(current_group) = self.active_group() else { return };

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

    fn open_tab(&mut self, group: usize, sender: &ComponentSender<Self>) {
        let terminal = Terminal::builder().hexpand(true).vexpand(true).build();
        apply_scheme(&terminal, self.style.is_dark());
        spawn_shell(&terminal);

        let id = self.next_tab_id;
        self.next_tab_id += 1;

        #[allow(deprecated)] // successor termprop API needs VTE >= 0.78 feature gates
        terminal.connect_window_title_notify({
            let sender = sender.clone();
            move |terminal| {
                if let Some(title) = terminal.window_title().filter(|t| !t.is_empty()) {
                    sender.input(Msg::TitleChanged(id, title.to_string()));
                }
            }
        });

        terminal.connect_child_exited({
            let sender = sender.clone();
            move |_, status| sender.input(Msg::ChildExited(id, status))
        });

        self.stack.add_child(&terminal);
        self.tabs.push(Tab {
            id,
            group,
            title: format!("Terminal {id}"),
            crashed: false,
            terminal,
        });
        self.activate(id);
    }

    fn activate(&mut self, id: usize) {
        if self.active == Some(id) {
            return; // also breaks the select_row → row-selected → Select cycle
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else { return };
        self.active = Some(id);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == tab.group) {
            group.last_active = id;
        }
        self.stack.set_visible_child(&tab.terminal);
        tab.terminal.grab_focus();
        self.rebuild_list();
    }

    fn close_tab(&mut self, id: usize) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else { return };
        let order = self.nav_order();
        let tab = self.tabs.remove(index);
        self.stack.remove(&tab.terminal);
        self.groups.retain(|g| self.tabs.iter().any(|t| t.group == g.id));

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
        let Some(pos) = order.iter().position(|&t| t == active) else { return };
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
        let Some(si) = self.tabs.iter().position(|t| t.id == src) else { return };
        let mut tab = self.tabs.remove(si);
        let Some(di) = self.tabs.iter().position(|t| t.id == dest) else {
            self.tabs.insert(si, tab);
            return;
        };
        tab.group = self.tabs[di].group;
        let moved_group = tab.group;
        self.tabs.insert(di, tab);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == moved_group) {
            if self.active == Some(src) {
                group.last_active = src;
            }
        }
        self.groups.retain(|g| self.tabs.iter().any(|t| t.group == g.id));
        self.rebuild_list();
    }

    /// Jump to the first tab of the next/previous group, wrapping around.
    fn navigate_group(&mut self, step: isize) {
        let Some(current) = self.active_group() else { return };
        let Some(pos) = self.groups.iter().position(|g| g.id == current) else { return };
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
            if tab.crashed {
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

    fn make_row(&self, tab: &Tab, group: &Group, collapsed_count: Option<usize>) -> gtk::ListBoxRow {
        let label = match collapsed_count {
            Some(n) => format!("{} ({n})", tab.title),
            None => tab.title.clone(),
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
        if tab.crashed {
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

        let drop = gtk::DropTarget::new(
            gtk::glib::Type::STRING,
            gtk::gdk::DragAction::MOVE,
        );
        let dest_id = tab.id;
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(src) = value.get::<String>() else { return false };
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
        if !visible {
            if let Some(tab) = self.active_tab() {
                tab.terminal.grab_focus();
            }
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
