use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use relm4::adw;
use relm4::adw::prelude::{AdwDialogExt, AlertDialogExt, PreferencesGroupExt};
use relm4::gtk;
use relm4::gtk::gdk::RGBA;
use relm4::gtk::gio;
use relm4::gtk::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt, SimpleComponent};
use vte4::{PtyFlags, Terminal, TerminalExt, TerminalExtManual};

use crate::browser::{self, Browser, CapturedFrame, ProfileDisposition};
use crate::control;
use crate::state::{self, SavedGroup, SavedState, SavedTab};
use crate::tmuxctl::{self, LingerStatus, SessionInfo, TmuxAvailability, TmuxCtl, TmuxError};

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
        "Group settings",
        Msg::GroupSettingsDialog,
    ),
    ("<Control>Page_Down", "Next tab", Msg::NavNext),
    ("<Control>Page_Up", "Previous tab", Msg::NavPrev),
    ("<Alt>Page_Down", "Next group", Msg::GroupNext),
    ("<Alt>Page_Up", "Previous group", Msg::GroupPrev),
    ("<Alt>1", "Toggle tab pane", Msg::ToggleSidebar),
    ("<Alt><Shift>1", "Toggle tab pane", Msg::ToggleSidebar),
    ("<Alt>exclam", "Toggle tab pane", Msg::ToggleSidebar),
    ("<Alt>2", "Toggle browser pane", Msg::ToggleBrowser),
    ("<Alt><Shift>2", "Toggle browser pane", Msg::ToggleBrowser),
    ("<Alt>at", "Toggle browser pane", Msg::ToggleBrowser),
    ("<Alt>quotedbl", "Toggle browser pane", Msg::ToggleBrowser),
    ("F1", "Show this help", Msg::ShowHelp),
];

/// Triggers that exist only as keyboard-layout aliases of another entry; the
/// F1 help lists each shortcut once, so these are filtered out there.
const SHORTCUT_ALIASES: &[&str] = &[
    "<Alt><Shift>1",
    "<Alt>exclam",
    "<Alt><Shift>2",
    "<Alt>at",
    "<Alt>quotedbl",
];

/// How often the hosted Chromium processes are reaped for exit.
const BROWSER_POLL_SECS: u32 = 2;

/// CDP discovery: poll the profile's DevToolsActivePort file this often…
const CDP_POLL_MS: u64 = 100;
/// …and give up after this many attempts (10 s total).
const CDP_POLL_TRIES: u32 = 100;

/// Environment keys published into each tab's tmux session (see
/// docs/superpowers/specs/2026-07-30-cdp-endpoint-design.md).
const ENV_CDP: &str = "KABELSALAT_CDP";
const ENV_CDP_PLAYWRIGHT: &str = "PLAYWRIGHT_MCP_CDP_ENDPOINT";
const ENV_GROUP: &str = "KABELSALAT_GROUP";
/// The endpoint pair: set on discovery, unset on browser close. `ENV_GROUP`
/// is deliberately not in here — it describes the tab, not the browser, and
/// is never unset.
const CDP_ENV_KEYS: [&str; 2] = [ENV_CDP, ENV_CDP_PLAYWRIGHT];

/// How long the "hold Shift to select" icon stays up after a bare drag.
const SELECT_HINT_SECS: u32 = 7;

/// Frame-clock ticks to wait for a window allocation wide enough to hold a restored
/// browser split. `GtkPaned::set_position` clamps to the current width and keeps the
/// clamped value, so applying a split before the window has settled pins the pane to a
/// one-pixel sliver. ~1s at 60 Hz is far longer than a startup allocation takes.
const SPLIT_SETTLE_TICKS: u32 = 60;

/// Width the browser pane keeps when the window is too narrow for its stored split.
const MIN_BROWSER_PX: i32 = 200;

/// Drag distance, in pixels, below which a drag is treated as a shaky click
/// rather than an attempted selection.
const DRAG_THRESHOLD_PX: f64 = 8.0;

/// The Ctrl-V byte, written straight into the active tab's pty after a
/// screenshot lands on the clipboard.
///
/// Writing to the pty is what keeps tmux out of this feature entirely: the
/// byte behaves the same whether the pty runs a tmux client or a plain shell,
/// exactly as if the user had pressed the keys.
const CTRL_V: u8 = 0x16;

/// Floor for the *active* tab. It is deliberately a floor and not a pin: every
/// tab asks for its full title as natural width, so it renders unabbreviated
/// whenever the bar has the room, and only gives ground when the window is
/// genuinely too narrow. Pinning the minimum to the full width instead made
/// the bar's own minimum 474px wide and forced it to overflow in narrow
/// windows.
const ACTIVE_TAB_MIN_CHARS: i32 = 8;

/// Floor for inactive tab buttons: how narrow they may be squeezed before the
/// tab bar starts scrolling instead.
const TAB_MIN_CHARS: i32 = 3;

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
    /// Stable, never-reused identity of this group. Group *ids* are reused
    /// (`max(id) + 1` after a delete), so anything that outlives the group —
    /// above all the browser profile directory — is keyed by this instead.
    uuid: String,
    name: String, // empty = unnamed, no header shown
    css: &'static str,
    last_active: usize,
    /// The group's browser, if it has one. Dropping it kills Chromium, closes
    /// the pane and removes the profile directory (see `browser::Browser`).
    browser: Option<Browser>,
    /// Whether the browser is shown rather than hidden. Only meaningful while
    /// `browser` is `Some`; a hidden browser keeps running.
    browser_visible: bool,
    /// Divider position of the terminal/browser split, in pixels.
    browser_split: f64,
    /// Where a freshly launched browser for this group starts, already
    /// normalized. `None` = whatever Chromium opens on its own. Editing it never
    /// touches a running browser: it is a launch argument, and only a launch
    /// into a *fresh* profile uses it.
    default_url: Option<String>,
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
    /// Scroll wrapper around `tab_bar`; owns the horizontal adjustment used to
    /// keep the active tab on screen.
    tab_scroller: gtk::ScrolledWindow,
    stack: gtk::Stack,
    /// Splits the terminal content box (start) from the active group's browser
    /// pane (end). The end child is attached and detached imperatively — only
    /// the active group's pane is ever parented.
    browser_paned: gtk::Paned,
    /// Wraps the window content; carries the transient failure notices that do
    /// not deserve a dialog.
    toast_overlay: adw::ToastOverlay,
    /// Popover behind the header bar's browser overflow button.
    browser_menu: gtk::Popover,
    /// Endpoint row in the browser overflow menu: dimmed "CDP: unavailable"
    /// until discovery succeeds, then `127.0.0.1:<port>` plus a live copy
    /// button.
    cdp_label: gtk::Label,
    cdp_copy: gtk::Button,
    /// Which group's pane is currently the paned's end child, if any. Tracked
    /// explicitly so detaching works even after the group was pruned.
    attached_browser: Option<usize>,
    /// Groups still waiting for their browser to be restored after a restart,
    /// active group first. Drained one per idle callback.
    pending_browser_restore: Vec<usize>,
    /// Whether the try_wait poll timer is running (started with the first
    /// browser, never restarted).
    browser_poll_running: bool,
    /// Whether a restore failure was already reported; keeps a broken setup
    /// from stacking one dialog per group.
    browser_restore_error_shown: bool,
    window: gtk::ApplicationWindow,
    input: relm4::Sender<Msg>,
    /// tmux backing, present only when a usable tmux (>= 3.2) was found.
    tmux: Option<TmuxCtl>,
    /// Result of the one-time startup availability check (drives the warning).
    availability: TmuxAvailability,
    /// Startup linger check; drives the "won't survive logout" warning icon.
    linger: LingerStatus,
    /// Whether the linger warning was permanently dismissed (persisted).
    linger_dismissed: bool,
    /// Whether the "hold Shift to select" hint icon is currently showing.
    /// Transient: never persisted, re-armed on every bare drag.
    select_hint_visible: bool,
    /// Generation counter for the hint's hide timer. A drag during the visible
    /// window bumps it, so the earlier timeout fires into a stale generation
    /// and is ignored instead of hiding the icon early.
    select_hint_gen: u64,
    /// Watches the pane-died events dir; kept alive for its lifetime.
    monitor: Option<gio::FileMonitor>,
    /// Where the presentation model is persisted.
    state_path: PathBuf,
    /// The state directory; browser profiles live under `browsers/` in it.
    state_dir: PathBuf,
    /// Group uuids were minted during this run's `state::load` and have not
    /// reached disk yet. While this is set, the uuids are not stable across
    /// restarts, so nothing may be deleted on the strength of them — see
    /// [`profile_sweep_allowed`]. `Cell` because `save_state` takes `&self`.
    uuids_unpersisted: Cell<bool>,
    /// Whether the "could not save state" notice was already shown; keeps a
    /// full disk from stacking one dialog per layout change.
    save_error_shown: Cell<bool>,
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
    /// Reorder groups: move group `src` to sit before group `dest`.
    DropGroup {
        src: usize,
        dest: usize,
    },
    /// Move tab `src` into `group`, appended at the end of its tabs.
    DropTabOnGroup {
        src: usize,
        group: usize,
    },
    /// Open the group settings dialog for the active group.
    GroupSettingsDialog,
    /// Apply the group settings dialog: name and default URL land together, in
    /// one save. `default_url` is already normalized by the dialog.
    ApplyGroupSettings {
        id: usize,
        name: String,
        default_url: Option<String>,
    },
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
    /// Open the "shells won't survive logout" (linger) explanation dialog.
    ShowLingerWarning,
    /// Run `loginctl enable-linger`, re-check, and hide the icon on success.
    EnableLinger,
    /// Result of the off-thread `enable_linger` + re-check: the new linger
    /// status on success, or the failure message.
    LingerEnabled(Result<LingerStatus, String>),
    /// Permanently hide the linger warning (persists the dismissal flag).
    DismissLingerWarning,
    /// The user dragged in a terminal without Shift, so tmux ate the drag
    /// instead of VTE selecting text. Shows the hint icon.
    BareDragHint,
    /// The hint's display window elapsed; the payload is the generation it was
    /// armed for, so a superseded timer is a no-op.
    HideSelectHint(u64),
    /// Open the "hold Shift to select" explanation dialog.
    ShowSelectHelp,
    /// Alt-2: spawn the active group's browser if it has none, otherwise
    /// toggle it between shown and hidden.
    ToggleBrowser,
    /// Tear down the active group's browser: kill Chromium, close the pane,
    /// remove the profile directory. The next Alt-2 spawns a fresh one.
    CloseBrowser,
    /// The Chromium hosted by this group's browser exited on its own. Tears
    /// down like `CloseBrowser`, but keeps the profile directory so the
    /// session survives the crash.
    BrowserDied(usize),
    /// The user dragged the terminal/browser divider.
    BrowserSplitChanged,
    /// Timer tick: reap exited Chromium processes.
    PollBrowsers,
    /// Restart restore: bring up this group's browser, then queue the next.
    RestoreBrowser(usize),
    /// CDP discovery finished for this group's browser: the endpoint, or
    /// `None` when `DevToolsActivePort` never appeared or never parsed.
    CdpReady(usize, Option<browser::CdpEndpoint>),
    /// Copy the active group's CDP endpoint URL to the clipboard.
    CopyCdpEndpoint,
    /// Capture what the active group's browser shows.
    Screenshot,
    /// The capture finished: the frame, or why there is none.
    ///
    /// The error is already rendered to text, like [`Msg::LingerEnabled`]'s:
    /// klamottenkiste's `CaptureError` is not `Clone`, and both failure kinds
    /// mean the same thing here — a toast and a line on stderr.
    ScreenshotCaptured(Result<CapturedFrame, String>),
    /// A `kabelsalat run` invocation: create a tab in the group with this
    /// uuid, running `argv` in `cwd`. Deliberately inert otherwise — it does
    /// not activate the tab, raise the window, change the active group, or
    /// touch the group's browser pane, because the user may be typing
    /// somewhere else when an agent fires this.
    SpawnCommand {
        group_uuid: String,
        tab_uuid: String,
        cwd: PathBuf,
        argv: Vec<String>,
    },
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

                pack_end = &gtk::Button {
                    set_icon_name: "dialog-warning-symbolic",
                    add_css_class: "tmux-warning",
                    set_tooltip_text: Some("Shells won't survive logout — click for details"),
                    #[watch]
                    set_visible: model.tmux.is_some()
                        && model.linger == LingerStatus::Disabled
                        && !model.linger_dismissed,
                    connect_clicked => Msg::ShowLingerWarning,
                },

                pack_end = &gtk::Button {
                    set_icon_name: "dialog-information-symbolic",
                    add_css_class: "select-hint",
                    set_tooltip_text: Some("Hold Shift to select text — click for details"),
                    #[watch]
                    set_visible: model.select_hint_visible,
                    connect_clicked => Msg::ShowSelectHelp,
                },

                pack_end = &gtk::Button {
                    set_icon_name: "web-browser-symbolic",
                    add_css_class: "browser-hidden",
                    set_tooltip_text: Some("Browser hidden — click to show it (Alt+2)"),
                    #[watch]
                    set_visible: model.active_browser_hidden(),
                    connect_clicked => Msg::ToggleBrowser,
                },

                pack_end = &gtk::Button {
                    set_icon_name: "camera-photo-symbolic",
                    set_tooltip_text: Some("Screenshot browser → paste into terminal"),
                    #[watch]
                    set_sensitive: model.active_browser_running(),
                    connect_clicked => Msg::Screenshot,
                },

                pack_end = &gtk::MenuButton {
                    set_icon_name: "view-more-symbolic",
                    set_tooltip_text: Some("Browser options"),
                    #[watch]
                    set_visible: model.active_group_has_browser(),

                    #[wrap(Some)]
                    set_popover = &browser_menu.clone() {
                        #[wrap(Some)]
                        set_child = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 6,

                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 6,
                                append: &cdp_label.clone(),
                                append: &cdp_copy.clone(),
                            },

                            gtk::Button {
                                set_label: "Close browser",
                                add_css_class: "flat",
                                connect_clicked[sender, browser_menu] => move |_| {
                                    browser_menu.popdown();
                                    sender.input(Msg::CloseBrowser);
                                },
                            },
                        },
                    },
                },
            },

            // The toast overlay wraps the whole content: a screenshot that
            // could not be taken is worth a line the user notices, but not a
            // dialog they have to dismiss.
            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                #[wrap(Some)]
                set_child = &gtk::Paned {
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

                    // The browser split. Its end child is the active group's
                    // WaylandPane, attached and detached imperatively; with no
                    // browser showing, the terminal box takes the full width.
                    #[wrap(Some)]
                    set_end_child = &browser_paned.clone() {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_resize_end_child: false,
                        set_shrink_end_child: false,

                    #[wrap(Some)]
                    set_start_child = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_hexpand: true,

                        // The scroller is what lets the bar have a small minimum
                        // width: past the point where inactive tabs hit their
                        // character floor, the overflow scrolls instead of forcing
                        // the window wider.
                        append = &tab_scroller.clone() {
                            set_hscrollbar_policy: gtk::PolicyType::External,
                            set_vscrollbar_policy: gtk::PolicyType::Never,
                            set_propagate_natural_height: true,
                            set_visible: false,

                            #[wrap(Some)]
                            set_child = &tab_bar.clone() {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 2,
                                set_margin_all: 4,
                            },
                        },

                        append = &stack.clone() {
                            set_hexpand: true,
                            set_vexpand: true,
                        },
                    },
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
                Ok(ctl) => {
                    // Start the server detached so sessions can survive logout
                    // (best-effort; failure degrades to the attached fallback).
                    if let Err(err) = ctl.ensure_server(tmuxctl::has_systemd_run()) {
                        eprintln!(
                            "tmux start-server failed, sessions may not survive logout: {err}"
                        );
                    }
                    Some(ctl)
                }
                Err(err) => {
                    eprintln!("tmux setup failed, using direct shells: {err}");
                    None
                }
            },
            _ => None,
        };

        // Logout survival is only relevant with a usable tmux backing.
        let linger = if tmux.is_some() {
            tmuxctl::detect_linger()
        } else {
            LingerStatus::NotApplicable
        };
        // Load the persisted dismissal flag before the view is built so the
        // icon's #[watch] visibility is correct on first render.
        let linger_dismissed = state::load(&state::state_file()).linger_warning_dismissed;

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
            tab_scroller: gtk::ScrolledWindow::new(),
            stack: gtk::Stack::new(),
            browser_paned: gtk::Paned::new(gtk::Orientation::Horizontal),
            toast_overlay: adw::ToastOverlay::new(),
            browser_menu: gtk::Popover::new(),
            cdp_label: gtk::Label::builder()
                .label("CDP: unavailable")
                .css_classes(["monospace", "dim-label"])
                .build(),
            cdp_copy: gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy CDP endpoint URL")
                .css_classes(["flat"])
                .sensitive(false)
                .build(),
            attached_browser: None,
            pending_browser_restore: Vec::new(),
            browser_poll_running: false,
            browser_restore_error_shown: false,
            window: root.clone(),
            input: sender.input_sender().clone(),
            tmux,
            availability,
            linger,
            linger_dismissed,
            select_hint_visible: false,
            select_hint_gen: 0,
            monitor: None,
            state_path: state::state_file(),
            state_dir: state::state_dir(),
            // Set for real in `restore_or_fresh`, which is the load that the
            // model is actually built from.
            uuids_unpersisted: Cell::new(false),
            save_error_shown: Cell::new(false),
        };

        let tab_list = model.tab_list.clone();
        let tab_bar = model.tab_bar.clone();
        let tab_scroller = model.tab_scroller.clone();
        let stack = model.stack.clone();
        let browser_paned = model.browser_paned.clone();
        let toast_overlay = model.toast_overlay.clone();
        let browser_menu = model.browser_menu.clone();
        let cdp_label = model.cdp_label.clone();
        let cdp_copy = model.cdp_copy.clone();
        cdp_copy.connect_clicked({
            let sender = sender.clone();
            let browser_menu = browser_menu.clone();
            move |_| {
                browser_menu.popdown();
                sender.input(Msg::CopyCdpEndpoint);
            }
        });
        let widgets = view_output!();

        // The divider lives in the widget; mirror it into the active group as
        // it moves so quitting right after a resize keeps the new position.
        {
            let input = sender.input_sender().clone();
            model.browser_paned.connect_position_notify(move |_| {
                let _ = input.send(Msg::BrowserSplitChanged);
            });
        }

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

        control::register(sender.input_sender().clone());
        // `restore_or_fresh` runs `start_browser_maintenance` itself, on both
        // of its exits, *before* it persists — otherwise that first save would
        // write browser_open=false for every restored group (no `Browser` yet,
        // `pending_browser_restore` not yet filled), and a crash inside that
        // window would lose every browser flag.
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
                    let uuid_group = self
                        .tabs
                        .iter()
                        .find(|t| t.id == id)
                        .map(|t| (t.uuid.clone(), t.group));
                    if let Some((uuid, group)) = uuid_group {
                        // Only a *definitive* empty result proves the session
                        // is gone. A query error (transient fork failure,
                        // busy server, novel stderr) means liveness is unknown
                        // — treat it conservatively as still alive and reattach
                        // (`new-session -A` is idempotent), because close_tab
                        // would kill a possibly-running shell and lose it.
                        let gone = session_definitively_gone(&tmux.list_sessions(), &uuid);
                        if gone {
                            self.close_tab(id);
                        } else {
                            // Reattach: -A ignores -c, and we want the
                            // existing session's directory anyway, so no cwd.
                            // -e is ignored on a genuine reattach too — but if
                            // list_sessions errored while the session was
                            // actually dead, -A creates a session here, and
                            // these pairs are what it needs.
                            let group_info = self.group_env_pairs(group);
                            let mut env: Vec<(&str, &str)> = Vec::new();
                            if let Some((group_uuid, cdp_url)) = &group_info {
                                env.push((ENV_GROUP, group_uuid.as_str()));
                                if let Some(url) = cdp_url {
                                    for key in CDP_ENV_KEYS {
                                        env.push((key, url.as_str()));
                                    }
                                }
                            }
                            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                                spawn_backing(&tab.terminal, &uuid, Some(tmux), None, None, &env);
                            }
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
            Msg::ShowLingerWarning => self.show_linger_warning(),
            Msg::EnableLinger => self.enable_linger(),
            Msg::LingerEnabled(result) => self.on_linger_enabled(result),
            Msg::DismissLingerWarning => self.linger_dismissed = true,
            // The three hint messages are pure transient UI and fire as often
            // as the user drags, so they return early to skip the save_state()
            // at the bottom rather than rewriting the state file per drag.
            Msg::BareDragHint => {
                self.show_select_hint();
                return;
            }
            Msg::HideSelectHint(generation) => {
                if generation == self.select_hint_gen {
                    self.select_hint_visible = false;
                }
                return;
            }
            Msg::ShowSelectHelp => {
                self.show_select_help();
                return;
            }
            Msg::ToggleBrowser => self.toggle_browser(),
            Msg::CloseBrowser => {
                if let Some(group) = self.active_group() {
                    // Deliberate close: the user is done with this browser, so
                    // the profile goes with it.
                    self.close_browser(group, ProfileDisposition::Remove);
                }
            }
            // Unexpected death: keep the profile so the next Alt-2 relaunches
            // into it and Chromium restores the session, cookies and logins.
            Msg::BrowserDied(group) => self.close_browser(group, ProfileDisposition::Keep),
            Msg::BrowserSplitChanged => {
                self.on_browser_split_changed();
                return;
            }
            // Pure bookkeeping: any actual death comes back as BrowserDied,
            // which persists. Ticking every BROWSER_POLL_SECS seconds must not
            // rewrite state.
            Msg::PollBrowsers => {
                self.poll_browsers();
                return;
            }
            Msg::RestoreBrowser(group) => self.restore_browser(group),
            // Runtime state only: nothing here is persisted, so return early
            // to skip the save_state() at the bottom (like PollBrowsers).
            Msg::CdpReady(group_id, endpoint) => {
                let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) else {
                    return;
                };
                let Some(browser) = group.browser.as_mut() else {
                    return;
                };
                match endpoint {
                    Some(endpoint) => {
                        let url = endpoint.url();
                        browser.set_cdp(endpoint);
                        self.cdp_env_set(group_id, &url);
                    }
                    None => {
                        eprintln!("kabelsalat: no CDP endpoint for group {group_id} within 10 s");
                    }
                }
                self.refresh_cdp_menu();
                return;
            }
            Msg::CopyCdpEndpoint => {
                let url = self
                    .active_group()
                    .and_then(|id| self.groups.iter().find(|g| g.id == id))
                    .and_then(|g| g.browser.as_ref())
                    .and_then(|b| b.cdp_url());
                if let (Some(url), Some(display)) = (url, gtk::gdk::Display::default()) {
                    display.clipboard().set_text(&url);
                }
                return;
            }
            // The clipboard and the pty are the only things this touches, and
            // neither is persisted — so both arms return early like the other
            // runtime-only messages.
            Msg::Screenshot => {
                self.capture_browser(&sender);
                return;
            }
            Msg::ScreenshotCaptured(result) => {
                match result {
                    Ok(frame) => self.paste_screenshot(frame),
                    Err(detail) => {
                        eprintln!("kabelsalat: browser screenshot failed: {detail}");
                        self.show_toast("The browser screenshot failed.");
                    }
                }
                return;
            }
            // Title changes arrive at animation rate from some programs
            // (spinners/progress in the title). Rebuilding the sidebar and
            // tab bar per change kept the main thread busy and replaced the
            // very buttons a click was landing on, swallowing the click. So:
            // touch only the affected labels, and early-return to skip the
            // state-file write — the next layout mutation persists the title.
            Msg::TitleChanged(id, title) => {
                self.update_title(id, title);
                return;
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
            Msg::DropGroup { src, dest } => self.drop_group(src, dest),
            Msg::DropTabOnGroup { src, group } => self.drop_tab_on_group(src, group),
            Msg::GroupSettingsDialog => self.show_group_settings_dialog(),
            Msg::ApplyGroupSettings {
                id,
                name,
                default_url,
            } => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    // An empty name still means an unnamed group with no header.
                    group.name = name.trim().to_string();
                    group.default_url = default_url;
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
            Msg::SpawnCommand {
                group_uuid,
                tab_uuid,
                cwd,
                argv,
            } => {
                // The snapshot the CLI validated against is a copy, so the
                // group can in principle be gone by the time this arrives.
                let Some(group_id) = self
                    .groups
                    .iter()
                    .find(|g| g.uuid == group_uuid)
                    .map(|g| g.id)
                else {
                    eprintln!("spawn request for unknown group {group_uuid}");
                    return;
                };
                let title = crate::cli::command_title(&argv);
                // Without tmux, VTE's spawn just fails for a nonexistent
                // directory and the callback only logs to stderr, leaving a
                // permanently empty tab even though the CLI already printed a
                // uuid and exited 0. Drop the cwd so the tab starts in the
                // default location instead (tmux itself tolerates a missing
                // -c directory, so this only matters for the no-tmux path).
                let cwd = cwd.is_dir().then_some(cwd);
                self.add_tab(
                    tab_uuid,
                    group_id,
                    Some(title),
                    None,
                    cwd.as_deref(),
                    Some(&argv),
                    &sender,
                );
                // add_tab alone leaves the sidebar stale; the usual funnel for
                // that is activate(), which we deliberately do not call. The
                // bottom-of-update save_state() below still runs, since this
                // arm does not return early.
                self.rebuild_list();
            }
        }
        // Every layout mutation persists; writes are atomic and human-paced.
        self.save_state();
    }

    /// On app exit, kill every hosted Chromium but keep its profile directory:
    /// the group still exists in the saved state and its browser is restored
    /// on the next start from exactly that profile.
    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        // Read the live divider back into its group *before* detaching, then
        // clear the attachment so the position notify from unparenting cannot
        // overwrite it with a meaningless value.
        self.on_browser_split_changed();
        self.attached_browser = None;
        self.browser_paned.set_end_child(gtk::Widget::NONE);
        // Sessions outlive the app; this is the one moment the app knows the
        // endpoints are dying, so unset them before the browsers are killed
        // rather than leaving the pair to rot in surviving sessions until the
        // next startup's refresh.
        let browser_group_ids: Vec<usize> = self
            .groups
            .iter()
            .filter(|g| g.browser.is_some())
            .map(|g| g.id)
            .collect();
        for id in browser_group_ids {
            self.cdp_env_unset(id);
        }
        // One shared SIGTERM deadline for all of them: serial teardown cost
        // `n * TERM_GRACE` of frozen UI, this costs at most TERM_GRACE however
        // many browsers there are. `Keep` preserves every profile for restore.
        browser::shutdown_all(
            self.groups.iter_mut().filter_map(|g| g.browser.as_mut()),
            browser::TERM_GRACE,
            ProfileDisposition::Keep,
        );
        // `update()` does not run on the way out, so persist here: without
        // this, a resize followed by a quit loses the new divider position.
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
            uuid: state::new_uuid(),
            name: String::new(),
            css: GROUP_PALETTE[(id - 1) % GROUP_PALETTE.len()],
            last_active: 0,
            browser: None,
            browser_visible: false,
            browser_split: state::DEFAULT_BROWSER_SPLIT,
            default_url: None,
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
        let loaded = state::load_detailed(&self.state_path);
        let saved = loaded.state;
        self.uuids_unpersisted.set(loaded.uuids_backfilled);
        // Get freshly minted uuids onto disk *now*, before anything keys off
        // them. Written verbatim: this is exactly the file that was read, plus
        // the uuids, so it cannot lose anything the model has not seen yet. If
        // it fails, `uuids_unpersisted` stays set and the profile sweep below
        // stands down instead of deleting every profile it cannot match.
        if loaded.uuids_backfilled {
            self.persist(&saved);
        }
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
            // No group ever had a browser here, but the stale-profile sweep
            // must still run.
            self.start_browser_maintenance();
            self.save_state();
            return;
        }

        // Recreate saved groups; keep the id allocator above every restored id.
        for group in &saved.groups {
            self.groups.push(Group {
                id: group.id,
                // Backfilled by `state::load` for legacy/duplicate entries, so
                // this is always a unique, non-empty uuid.
                uuid: group.uuid.clone(),
                name: group.name.clone(),
                css: GROUP_PALETTE[group.palette % GROUP_PALETTE.len()],
                last_active: 0,
                browser: None,
                // Restored as "wanted"; the actual pane comes up later, one
                // group at a time, in start_browser_maintenance().
                browser_visible: group.browser_open,
                browser_split: group.browser_split,
                default_url: group.default_url.clone(),
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
                None,
                None,
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
                    None,
                    None,
                    sender,
                );
            }
        }

        // Reattached sessions were not touched by -e (a -A attach ignores
        // it) and may carry stale endpoint variables from a previous run —
        // or none of the variables at all, if written by an older version.
        // This refresh is the only point where they can be brought under the
        // invariant: variables present in a session always describe a live
        // browser, and every session names its group. This must run before
        // start_browser_maintenance() below queues any browser restore, so
        // it always unsets stale endpoints rather than racing a fresh
        // discovery.
        let reattached: Vec<(String, usize)> = self
            .tabs
            .iter()
            .filter(|t| live.contains(&t.uuid))
            .map(|t| (t.uuid.clone(), t.group))
            .collect();
        for (uuid, group) in &reattached {
            self.session_env_refresh(uuid, *group);
        }

        // Drop any group that ended up empty, then restore the active tab.
        self.prune_empty_groups();
        let active_id = saved
            .active
            .as_ref()
            .and_then(|u| self.tabs.iter().find(|t| t.uuid == *u).map(|t| t.id))
            .or_else(|| self.tabs.first().map(|t| t.id));
        if let Some(id) = active_id {
            self.activate(id);
        }
        // Before the save, not after: this fills `pending_browser_restore`, so
        // the state written below already records every restored group as
        // browser_open (see `browser_open_desired`). Reversing the order would
        // persist browser_open=false for all of them until the first
        // RestoreBrowser save.
        self.start_browser_maintenance();
        self.save_state();
    }

    /// Serialize the current presentation model to disk (atomic write).
    fn save_state(&self) {
        // The attached group's divider lives in the widget, not the model, so
        // read it back here rather than on every drag of the handle.
        let live_split = self.browser_paned.position() as f64;
        let groups = self
            .groups
            .iter()
            .map(|g| SavedGroup {
                uuid: g.uuid.clone(),
                id: g.id,
                name: g.name.clone(),
                palette: GROUP_PALETTE.iter().position(|c| *c == g.css).unwrap_or(0),
                // The DESIRED state, not the live one: restore is sequential
                // and idle-driven, so a group still queued in
                // `pending_browser_restore` has no `Browser` yet but must
                // still be written as open — otherwise any save inside the
                // restore window silently forgets it.
                browser_open: browser_open_desired(
                    g.browser.is_some(),
                    &self.pending_browser_restore,
                    g.id,
                ),
                browser_split: if self.attached_browser == Some(g.id) {
                    live_split
                } else {
                    g.browser_split
                },
                default_url: g.default_url.clone(),
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
            linger_warning_dismissed: self.linger_dismissed,
        };
        self.persist(&state);
        // Same choke point as the save, so the CLI always sees what was last
        // written rather than a separately maintained copy.
        control::publish(
            state
                .groups
                .iter()
                .map(|g| crate::cli::GroupInfo {
                    uuid: g.uuid.clone(),
                    name: g.name.clone(),
                    tabs: state.tabs.iter().filter(|t| t.group == g.id).count(),
                })
                .collect(),
        );
    }

    /// Write `state` to disk and account for the outcome. A success clears
    /// `uuids_unpersisted` — whatever uuids the model carries are now on disk.
    /// A failure keeps the flag (so no profile sweep will trust them) and, on
    /// its first occurrence, tells the user, like every other browser-affecting
    /// failure does.
    fn persist(&self, state: &SavedState) {
        match state::save(state, &self.state_path) {
            Ok(()) => self.uuids_unpersisted.set(false),
            Err(err) => {
                eprintln!("failed to save state: {err}");
                if !self.save_error_shown.replace(true) {
                    self.show_notice(&format!(
                        "Could not save the session layout: {err}\n\n\
                         Tabs and browser sessions will not be restored \
                         correctly after a restart."
                    ));
                }
            }
        }
    }

    // ---- browser pane ---------------------------------------------------

    /// Does the active group have a browser at all? Drives the overflow menu.
    fn active_group_has_browser(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.browser.is_some())
    }

    /// Does the active group have a browser that is currently hidden? Drives
    /// the header-bar indicator.
    fn active_browser_hidden(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.browser.is_some() && !g.browser_visible)
    }

    /// Can the active group's browser be captured right now? Drives the
    /// screenshot button. The pane's compositor is what renders the frame, so
    /// a browser whose pane has died has nothing left to show — and a hidden
    /// browser has, which is why this asks "running", not "visible".
    fn active_browser_running(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .and_then(|g| g.browser.as_ref())
            .is_some_and(Browser::is_running)
    }

    /// Ask the active group's browser for a fresh frame. The pane answers on
    /// the GTK main context, so the reply comes back as an ordinary message.
    fn capture_browser(&self, sender: &ComponentSender<Self>) {
        let Some(browser) = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .and_then(|g| g.browser.as_ref())
        else {
            return;
        };
        let sender = sender.clone();
        browser.capture_frame(move |result| {
            sender.input(Msg::ScreenshotCaptured(
                result.map_err(|err| err.to_string()),
            ));
        });
    }

    /// Put a captured frame on the clipboard, then press Ctrl-V in the active
    /// tab so a `claude` running there stages it as an image.
    ///
    /// The claim outlives the paste — GDK serializes the texture per requester
    /// — so the screenshot stays available for the user to paste anywhere else.
    /// The keystroke is deferred by one main-loop iteration because the claim
    /// only reaches the compositor when the loop next runs, and the reader in
    /// the tab must not beat it there.
    fn paste_screenshot(&self, frame: CapturedFrame) {
        if !frame_backs_texture(frame.width, frame.height, frame.stride, frame.rgba.len()) {
            eprintln!("kabelsalat: unusable screenshot frame: {frame:?}");
            self.show_toast("The browser screenshot failed.");
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let (width, height, stride) = (frame.width as i32, frame.height as i32, frame.stride);
        let texture = gtk::gdk::MemoryTexture::new(
            width,
            height,
            gtk::gdk::MemoryFormat::R8g8b8a8,
            &gtk::glib::Bytes::from_owned(frame.rgba),
            stride,
        );
        display.clipboard().set_texture(&texture);

        // No active tab: nothing to type into, but the clipboard is set, so the
        // screenshot is still there to be pasted by hand.
        let Some(tab) = self.active_tab() else {
            return;
        };
        let terminal = tab.terminal.clone();
        gtk::glib::idle_add_local_once(move || {
            terminal.feed_child(&[CTRL_V]);
        });
    }

    /// Say something transient in the toast overlay.
    fn show_toast(&self, message: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(message));
    }

    /// Reflect the active group's CDP state in the overflow menu. The label
    /// shows host:port; the copy button carries the full http:// URL.
    fn refresh_cdp_menu(&self) {
        let url = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .and_then(|g| g.browser.as_ref())
            .and_then(|b| b.cdp_url());
        match url {
            Some(url) => {
                self.cdp_label.set_label(url.trim_start_matches("http://"));
                self.cdp_label.remove_css_class("dim-label");
                self.cdp_copy.set_sensitive(true);
            }
            None => {
                self.cdp_label.set_label("CDP: unavailable");
                self.cdp_label.add_css_class("dim-label");
                self.cdp_copy.set_sensitive(false);
            }
        }
    }

    /// Make the split show exactly the active group's browser, if it has a
    /// visible one. The outgoing pane's divider position is saved and the pane
    /// is hidden and unparented — the compositor and Chromium keep running, so
    /// coming back to the group is instant with page state intact.
    fn sync_browser_pane(&mut self) {
        // Refresh before the early return below: that return fires whenever
        // the *visible* pane isn't changing, but the active group itself may
        // still have changed (e.g. switching to a tab whose group's browser
        // is hidden), which the CDP menu must reflect regardless.
        self.refresh_cdp_menu();
        let want = self.active_group().filter(|id| {
            self.groups
                .iter()
                .any(|g| g.id == *id && g.browser.is_some() && g.browser_visible)
        });
        if self.attached_browser == want {
            return;
        }
        if let Some(previous) = self.attached_browser.take() {
            let split = self.browser_paned.position() as f64;
            if let Some(group) = self.groups.iter_mut().find(|g| g.id == previous) {
                group.browser_split = split;
                if let Some(browser) = &group.browser {
                    browser.set_visible(false);
                }
            }
            self.browser_paned.set_end_child(gtk::Widget::NONE);
        }
        if let Some(id) = want
            && let Some(group) = self.groups.iter().find(|g| g.id == id)
            && let Some(browser) = &group.browser
        {
            let split = group.browser_split as i32;
            let widget = browser.widget().clone();
            browser.set_visible(true);
            self.browser_paned.set_end_child(Some(&widget));
            self.apply_browser_split(split, widget.upcast());
            self.attached_browser = Some(id);
        }
    }

    /// Restore a split, waiting for an allocation that can actually hold it.
    ///
    /// `GtkPaned::set_position` clamps to the paned's *current* width and keeps the
    /// clamped value — it is not re-derived when the window later grows. Restoring a
    /// browser happens on an idle callback, before the window has its final size, so
    /// setting the position straight away pinned the pane to a one-pixel sliver that
    /// only a mouse drag could undo. Wait for a wide enough allocation instead.
    fn apply_browser_split(&self, split: i32, widget: gtk::Widget) {
        if split <= 0 {
            return;
        }
        let paned = self.browser_paned.clone();
        if paned.width() > split {
            paned.set_position(split);
            return;
        }
        let target = paned.clone();
        // `add_tick_callback` takes an `Fn`, so the counter needs interior mutability.
        let waited = Cell::new(0u32);
        paned.add_tick_callback(move |_, _| {
            // A group switch may have swapped this pane out meanwhile; that group's
            // own split owns the divider now.
            if target.end_child().as_ref() != Some(&widget) {
                return gtk::glib::ControlFlow::Break;
            }
            if target.width() > split {
                target.set_position(split);
                return gtk::glib::ControlFlow::Break;
            }
            waited.set(waited.get() + 1);
            if waited.get() < SPLIT_SETTLE_TICKS {
                return gtk::glib::ControlFlow::Continue;
            }
            // The window is genuinely narrower than the split was saved at, so the
            // stored value can never be honoured. Give the browser a usable strip
            // rather than leaving it a sliver — the same outcome a user would drag to.
            let width = target.width();
            if width > 0 {
                target.set_position((width - MIN_BROWSER_PX).max(0));
            }
            gtk::glib::ControlFlow::Break
        });
    }

    /// Hand the keyboard to whichever half the user just asked for.
    ///
    /// The pane mirrors its GTK focus onto the nested compositor's seat: losing
    /// GTK focus clears the seat's keyboard focus, and a pane without it
    /// receives no keys at all — the hosted browser goes deaf. Showing the
    /// browser is an explicit request to work in it, so it gets the keyboard;
    /// hiding it gives the keyboard back to the terminal.
    fn focus_browser_or_terminal(&self) {
        let browser = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .filter(|g| g.browser_visible)
            .and_then(|g| g.browser.as_ref());
        if let Some(browser) = browser {
            browser.widget().grab_focus();
        } else if let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
        {
            tab.terminal.grab_focus();
        }
    }

    /// Alt-2: no browser → spawn and show; hidden → show; shown → hide.
    /// Never panics: a failure to spawn leaves the group without a browser and
    /// reports the reason, mirroring how the app degrades without tmux.
    fn toggle_browser(&mut self) {
        let Some(id) = self.active_group() else {
            return;
        };
        let Some(group) = self.groups.iter().find(|g| g.id == id) else {
            return;
        };
        if group.browser.is_some() {
            let visible = group.browser_visible;
            if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                group.browser_visible = !visible;
            }
        } else {
            // Keyed by the group's uuid: ids are reused, so an id-keyed
            // profile could be one the startup sweep is still deleting.
            let uuid = group.uuid.clone();
            // Cloned like the uuid, so no borrow of `self.groups` is alive
            // across the spawn.
            let default_url = group.default_url.clone();
            match Browser::spawn(&uuid, &self.state_dir, default_url.as_deref()) {
                Ok(browser) => {
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                        group.browser = Some(browser);
                        group.browser_visible = true;
                    }
                    self.start_browser_poll();
                    self.start_cdp_poll(id);
                }
                Err(err) => {
                    self.show_notice(&err.to_string());
                    return;
                }
            }
        }
        self.sync_browser_pane();
        self.focus_browser_or_terminal();
    }

    /// Tear a group's browser down: terminate and reap Chromium, close the
    /// pane, then either remove the profile directory (deliberate close) or
    /// keep it (unexpected death, so the session can come back).
    ///
    /// Idempotent: a second call for a group whose browser is already gone —
    /// e.g. a duplicate `BrowserDied` from the exit poll — is a no-op.
    fn close_browser(&mut self, id: usize, disposition: ProfileDisposition) {
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return;
        };
        let Some(mut browser) = group.browser.take() else {
            return;
        };
        group.browser_visible = false;
        // NLL: `group` is done; &self calls are fine from here. Runs only
        // when a browser was actually present, so a duplicate BrowserDied
        // (see the doc comment above) stays a true no-op.
        self.cdp_env_unset(id);
        if self.attached_browser == Some(id) {
            self.browser_paned.set_end_child(gtk::Widget::NONE);
            self.attached_browser = None;
        }
        browser.teardown(disposition);
        drop(browser);
        self.sync_browser_pane();
    }

    /// Remember the divider position of the split the user just dragged, on
    /// the group it belongs to. Persisting happens with the next layout
    /// mutation or at shutdown, not per drag step.
    fn on_browser_split_changed(&mut self) {
        let Some(id) = self.attached_browser else {
            return;
        };
        let position = self.browser_paned.position() as f64;
        if position <= 0.0 {
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.browser_split = position;
        }
    }

    /// Start the exit poll once, on the first browser. klamottenkiste cannot
    /// report the hosted client's exit, so `try_wait` on a timer is the only
    /// way to notice a crash or the user quitting from inside Chromium.
    fn start_browser_poll(&mut self) {
        if self.browser_poll_running {
            return;
        }
        self.browser_poll_running = true;
        let input = self.input.clone();
        gtk::glib::timeout_add_seconds_local(BROWSER_POLL_SECS, move || {
            match input.send(Msg::PollBrowsers) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                // The component is gone; stop ticking.
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
    }

    /// Publish the endpoint pair to every session in the group. One session
    /// failing must not stop the others, and a tmux failure must never keep
    /// the browser from working: log and continue.
    fn cdp_env_set(&self, group_id: usize, url: &str) {
        let Some(tmux) = &self.tmux else { return };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in CDP_ENV_KEYS {
                if let Err(err) = tmux.set_environment(&tab.uuid, key, url) {
                    eprintln!("kabelsalat: set {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Remove the endpoint pair from every session in the group, so a shell
    /// started later does not inherit a dead endpoint.
    fn cdp_env_unset(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in CDP_ENV_KEYS {
                if let Err(err) = tmux.unset_environment(&tab.uuid, key) {
                    eprintln!("kabelsalat: unset {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Make one session's environment describe its group: `KABELSALAT_GROUP`
    /// always (a tab always belongs to some group — never unset), the
    /// endpoint pair set or unset by whether the group's browser has a live
    /// endpoint. Used when a session is (re)created and when a tab moves.
    fn session_env_refresh(&self, tab_uuid: &str, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return;
        };
        if let Err(err) = tmux.set_environment(tab_uuid, ENV_GROUP, &group.uuid) {
            eprintln!("kabelsalat: set {ENV_GROUP} on {tab_uuid}: {err}");
        }
        let url = group.browser.as_ref().and_then(|b| b.cdp_url());
        for key in CDP_ENV_KEYS {
            let result = match &url {
                Some(url) => tmux.set_environment(tab_uuid, key, url),
                // Also the startup path: a reattached session may carry a
                // stale endpoint written by a previous run (or version), and
                // this unset is the only point where it can be cleared.
                None => tmux.unset_environment(tab_uuid, key),
            };
            if let Err(err) = result {
                eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
            }
        }
    }

    /// The env pairs a session of this group is created with: the group
    /// identity always, the endpoint pair when the browser already has one.
    /// Returns None only for a group id that no longer exists.
    fn group_env_pairs(&self, group_id: usize) -> Option<(String, Option<String>)> {
        self.groups
            .iter()
            .find(|g| g.id == group_id)
            .map(|g| (g.uuid.clone(), g.browser.as_ref().and_then(|b| b.cdp_url())))
    }

    /// Watch for `DevToolsActivePort` in the group's profile. The file shows
    /// up roughly half a second after spawn, so this cannot block the main
    /// thread; stat-ing a file is cheap enough for a 100 ms local timeout
    /// (same reasoning as the BROWSER_POLL_SECS timer, not the linger thread).
    fn start_cdp_poll(&self, group_id: usize) {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return;
        };
        if group.browser.is_none() {
            return;
        }
        let profile = browser::profile_dir(&self.state_dir, &group.uuid);
        let port_file = browser::devtools_port_path(&profile);
        let input = self.input.clone();
        let mut tries = 0u32;
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(CDP_POLL_MS), move || {
            tries += 1;
            if let Ok(contents) = std::fs::read_to_string(&port_file)
                && let Some(endpoint) = browser::parse_devtools_active_port(&contents)
            {
                let _ = input.send(Msg::CdpReady(group_id, Some(endpoint)));
                return gtk::glib::ControlFlow::Break;
            }
            // Profile gone = browser closed mid-poll; stop quietly. The
            // CdpReady(None) on timeout is still delivered so the failure is
            // logged in exactly one place, the handler.
            if !profile.is_dir() {
                return gtk::glib::ControlFlow::Break;
            }
            if tries >= CDP_POLL_TRIES {
                let _ = input.send(Msg::CdpReady(group_id, None));
                return gtk::glib::ControlFlow::Break;
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    /// Reap exited Chromium processes; each one comes back as `BrowserDied`.
    fn poll_browsers(&mut self) {
        let mut dead = Vec::new();
        for group in self.groups.iter_mut() {
            let id = group.id;
            if let Some(browser) = &mut group.browser {
                match browser.has_exited() {
                    Ok(true) => dead.push(id),
                    Ok(false) => {}
                    Err(err) => eprintln!("kabelsalat: browser {id} could not be polled: {err}"),
                }
            }
        }
        for id in dead {
            let _ = self.input.send(Msg::BrowserDied(id));
        }
    }

    /// Post-restore browser work: sweep stale profile directories on a worker
    /// thread (pure filesystem IO, no GTK), then bring restored browsers up
    /// sequentially, the active group first, one per idle callback so the
    /// window is interactive immediately.
    fn start_browser_maintenance(&mut self) {
        // Keyed by uuid: a never-reused key means a freshly created group can
        // never claim a directory this sweep is concurrently deleting.
        let live: HashSet<String> = self.groups.iter().map(|g| g.uuid.clone()).collect();
        let state_dir = self.state_dir.clone();
        // The sweep decides purely by uuid, so it is only safe while the uuids
        // it compares against are the ones the next start will see too.
        if profile_sweep_allowed(self.uuids_unpersisted.get()) {
            std::thread::spawn(move || {
                if let Err(err) = browser::sweep_profiles(&state_dir, &live) {
                    eprintln!("kabelsalat: stale browser profiles: {err}");
                }
            });
        } else {
            eprintln!(
                "kabelsalat: skipping the stale browser profile sweep — group ids \
                 could not be persisted, so no profile can be proven stale"
            );
        }

        let active = self.active_group();
        let mut pending: Vec<usize> = self
            .groups
            .iter()
            .filter(|g| g.browser_visible && g.browser.is_none())
            .map(|g| g.id)
            .collect();
        // The browser the user can actually see becomes usable first.
        pending.sort_by_key(|id| Some(*id) != active);
        // Only the active group's browser is shown; the rest come back hidden
        // and announce themselves with the header-bar icon.
        for group in self.groups.iter_mut() {
            if Some(group.id) != active {
                group.browser_visible = false;
            }
        }
        self.pending_browser_restore = pending;
        self.queue_browser_restore();
    }

    /// Ask for the next pending restore on an idle callback.
    fn queue_browser_restore(&self) {
        let Some(id) = self.pending_browser_restore.first().copied() else {
            return;
        };
        let input = self.input.clone();
        gtk::glib::idle_add_local_once(move || {
            let _ = input.send(Msg::RestoreBrowser(id));
        });
    }

    /// Bring one restored group's browser up, then queue the next. A group
    /// that vanished (or gained a browser) meanwhile is simply skipped.
    fn restore_browser(&mut self, id: usize) {
        self.pending_browser_restore.retain(|&g| g != id);
        let wanted = self
            .groups
            .iter()
            .find(|g| g.id == id && g.browser.is_none())
            .map(|g| (g.uuid.clone(), g.default_url.clone()));
        if let Some((uuid, default_url)) = wanted {
            match Browser::spawn(&uuid, &self.state_dir, default_url.as_deref()) {
                Ok(browser) => {
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                        group.browser = Some(browser);
                    }
                    self.start_browser_poll();
                    self.start_cdp_poll(id);
                }
                Err(err) => {
                    eprintln!("kabelsalat: restoring browser for group {id}: {err}");
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                        group.browser_visible = false;
                    }
                    // One dialog, however many groups fail for the same reason.
                    if !self.browser_restore_error_shown {
                        self.browser_restore_error_shown = true;
                        self.show_notice(&err.to_string());
                    }
                }
            }
        }
        self.sync_browser_pane();
        self.queue_browser_restore();
    }

    /// Show the "hold Shift to select" icon and (re)arm its hide timer.
    fn show_select_hint(&mut self) {
        self.select_hint_visible = true;
        self.select_hint_gen = self.select_hint_gen.wrapping_add(1);
        let generation = self.select_hint_gen;
        let input = self.input.clone();
        gtk::glib::timeout_add_seconds_local_once(SELECT_HINT_SECS, move || {
            let _ = input.send(Msg::HideSelectHint(generation));
        });
    }

    /// Explain why a plain drag doesn't select, and what the wheel does now.
    fn show_select_help(&self) {
        let dialog =
            adw::AlertDialog::new(Some("Hold Shift to select text"), Some(select_help_body()));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }

    /// Explain that crash-safe sessions need tmux >= 3.2, with install hints.
    fn show_tmux_warning(&self) {
        let body = tmux_warning_body(&self.availability);
        let dialog = adw::AlertDialog::new(Some("Crash-safe sessions unavailable"), Some(&body));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }

    /// Offer to enable lingering so shells survive logout, with honest
    /// downsides. Buttons: Enable / Not now / Don't show again.
    fn show_linger_warning(&self) {
        let dialog = adw::AlertDialog::new(
            Some("Keep shells running after logout?"),
            Some(&linger_warning_body()),
        );
        dialog.add_response("not-now", "Not now");
        dialog.add_response("dismiss", "Don't show again");
        dialog.add_response("enable", "Enable");
        dialog.set_response_appearance("enable", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("enable"));
        dialog.set_close_response("not-now");

        let input = self.input.clone();
        dialog.connect_response(Some("enable"), {
            let input = input.clone();
            move |_, _| {
                let _ = input.send(Msg::EnableLinger);
            }
        });
        dialog.connect_response(Some("dismiss"), move |_, _| {
            let _ = input.send(Msg::DismissLingerWarning);
        });
        dialog.present(Some(&self.window));
    }

    /// Run `loginctl enable-linger` and re-check, off the GTK main thread.
    /// `enable-linger` triggers a polkit action that may prompt for interactive
    /// authentication, which would freeze the UI if run synchronously here; the
    /// work happens on a background thread and its outcome comes back as
    /// `Msg::LingerEnabled`.
    fn enable_linger(&self) {
        let input = self.input.clone();
        std::thread::spawn(move || {
            let result = match tmuxctl::enable_linger() {
                Ok(()) => Ok(tmuxctl::detect_linger()),
                Err(err) => Err(err.to_string()),
            };
            let _ = input.send(Msg::LingerEnabled(result));
        });
    }

    /// Apply the outcome of the off-thread `enable_linger`. On success the
    /// re-check reports `Enabled` and the #[watch] hides the icon; failure (or
    /// an unconfirmed enable) shows a brief notice and leaves it visible.
    fn on_linger_enabled(&mut self, result: Result<LingerStatus, String>) {
        match result {
            Ok(status) => {
                self.linger = status;
                if self.linger != LingerStatus::Enabled {
                    self.show_notice("Lingering could not be confirmed as enabled.");
                }
            }
            Err(err) => self.show_notice(&format!("Enabling lingering failed: {err}")),
        }
    }

    /// A brief informational dialog with a single Close button.
    fn show_notice(&self, message: &str) {
        let dialog = adw::AlertDialog::new(None, Some(message));
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
        let moved_uuid = tab.uuid.clone();
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == target) {
            group.last_active = active;
        }
        // A moved tab's session must describe its new home; the old group's
        // endpoint may still point at a live browser — just not one visible
        // from here (see the spec's "Tab moved between groups").
        self.session_env_refresh(&moved_uuid, target);
        self.prune_empty_groups();
        self.rebuild_list();
        // The active tab changed groups without an activate(), so the pane
        // and CDP menu must reconcile here too.
        self.sync_browser_pane();
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
            let name = if group.name.is_empty() {
                // unnamed group: fall back to its last-active tab's title
                &members
                    .iter()
                    .find(|t| t.id == group.last_active)
                    .unwrap_or(&members[0])
                    .title
            } else {
                &group.name
            };
            let label = gtk::Label::builder()
                .label(format!("{} ({})", name, members.len()))
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
    /// The new shell inherits the working directory of the tab that is active
    /// right now — captured before `activate` moves focus to the new tab.
    fn open_tab(&mut self, group: usize, sender: &ComponentSender<Self>) {
        let cwd = self.active_tab_cwd();
        let uuid = gtk::glib::uuid_string_random().to_string();
        let id = self.add_tab(uuid, group, None, None, cwd.as_deref(), None, sender);
        self.activate(id);
    }

    /// Working directory of the currently active tab, for seeding a new tab.
    /// Prefers tmux's `pane_current_path`; falls back to the VTE terminal's
    /// `current-directory-uri` when there is no tmux backing. Any failure —
    /// no active tab, a tmux/parse error, or a directory that no longer
    /// exists — yields `None`, so the new shell simply starts in `$HOME`.
    fn active_tab_cwd(&self) -> Option<PathBuf> {
        let tab = self.active_tab()?;
        let path = match &self.tmux {
            Some(ctl) => ctl.pane_current_path(&tab.uuid).ok()?,
            None => {
                #[allow(deprecated)] // successor termprop API needs VTE >= 0.78 feature gates
                let uri = tab.terminal.current_directory_uri()?;
                tmuxctl::parse_file_uri(&uri)?
            }
        };
        path.is_dir().then_some(path)
    }

    /// Create a tab backed by `uuid` (spawning its tmux session, or a direct
    /// $SHELL in the fallback path) without changing the active tab. Returns
    /// the new tab id. `title = None` uses the default "Terminal N".
    /// `command = None` starts an interactive shell; `Some(argv)` runs that
    /// instead, which is what `kabelsalat run` uses.
    // Each parameter represents an independent fact about the tab being created.
    // Bundling them into a struct would add indirection at only three call sites
    // without improving clarity of intent.
    #[allow(clippy::too_many_arguments)]
    fn add_tab(
        &mut self,
        uuid: String,
        group: usize,
        title: Option<String>,
        crashed: Option<i32>,
        cwd: Option<&Path>,
        command: Option<&[String]>,
        sender: &ComponentSender<Self>,
    ) -> usize {
        // Fallback scrolling off: tmux keeps VTE permanently in the alternate
        // screen, where VTE would otherwise translate the wheel into cursor-up
        // /down keypresses that land in the shell. tmux owns the wheel now
        // (`set -g mouse on`), and VTE's own scrollback is empty regardless.
        let terminal = Terminal::builder()
            .hexpand(true)
            .vexpand(true)
            .enable_fallback_scrolling(false)
            .build();
        apply_scheme(&terminal, self.style.is_dark());
        // Stamped at session creation via new-session -e: the group identity
        // always, the endpoint pair when the group's browser already has one.
        // A -A reattach ignores -e; reattached sessions are refreshed
        // explicitly in restore_or_fresh instead.
        let group_info = self.group_env_pairs(group);
        let mut env: Vec<(&str, &str)> = Vec::new();
        if let Some((group_uuid, cdp_url)) = &group_info {
            env.push((ENV_GROUP, group_uuid.as_str()));
            if let Some(url) = cdp_url {
                for key in CDP_ENV_KEYS {
                    env.push((key, url.as_str()));
                }
            }
        }
        spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command, &env);

        attach_drag_hint(&terminal, sender);

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
            None => spawn_shell(&terminal, None, None),
        }
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.crashed = None;
        }
        self.rebuild_list();
    }

    /// Drop every group that has no tabs left. A pruned group's `Browser` goes
    /// with it (kill Chromium, close the pane, remove the profile dir), so its
    /// widget must be unparented first.
    fn prune_empty_groups(&mut self) {
        let live: HashSet<usize> = self.tabs.iter().map(|t| t.group).collect();
        if self.groups.iter().all(|g| live.contains(&g.id)) {
            return;
        }
        if let Some(attached) = self.attached_browser
            && !live.contains(&attached)
        {
            self.browser_paned.set_end_child(gtk::Widget::NONE);
            self.attached_browser = None;
        }
        // A group that lost its browser to an unexpected Chromium exit keeps
        // its profile while the group lives (so the next Alt-2 restores the
        // session). Once the group itself goes away there is no `Browser` left
        // to dispose of it, so reclaim it here — otherwise it would survive
        // until the next startup sweep.
        for uuid in reclaimable_profiles(
            self.groups
                .iter()
                .map(|g| (g.id, g.uuid.as_str(), g.browser.is_some())),
            &live,
        ) {
            browser::remove_profile_in_background(&browser::profile_dir(&self.state_dir, &uuid));
        }
        // One shared SIGTERM deadline for all doomed browsers. Dropping the
        // groups below would tear each one down serially instead, costing
        // `k * TERM_GRACE` of frozen UI when k groups empty at once; this
        // costs at most TERM_GRACE. `shutdown_all` leaves each `Browser`
        // already torn down, so the later `Drop` is a no-op.
        browser::shutdown_all(
            self.groups
                .iter_mut()
                .filter(|g| !live.contains(&g.id))
                .filter_map(|g| g.browser.as_mut()),
            browser::TERM_GRACE,
            ProfileDisposition::Remove,
        );
        self.groups.retain(|g| live.contains(&g.id));
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
        // The main funnel for swapping the parented browser pane. Not the
        // only one: a tab *move* changes the active group with no tab
        // change, so the move handlers re-sync themselves.
        self.sync_browser_pane();
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
        self.prune_empty_groups();

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
        let old_group = tab.group;
        tab.group = self.tabs[di].group;
        let moved_group = tab.group;
        let moved_uuid = tab.uuid.clone();
        self.tabs.insert(di, tab);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == moved_group)
            && self.active == Some(src)
        {
            group.last_active = src;
        }
        if moved_group != old_group {
            self.session_env_refresh(&moved_uuid, moved_group);
        }
        self.prune_empty_groups();
        self.rebuild_list();
        // No-op unless the dragged tab was the active one changing groups.
        self.sync_browser_pane();
    }

    /// Reorder groups by drag-and-drop: move `src` before `dest`. The render
    /// order is `self.groups` order, so reordering it persists via save_state.
    fn drop_group(&mut self, src: usize, dest: usize) {
        let mut order: Vec<usize> = self.groups.iter().map(|g| g.id).collect();
        reorder_groups(&mut order, src, dest);
        // Stable sort by the new position; a no-op reorder leaves it untouched.
        self.groups
            .sort_by_key(|g| order.iter().position(|&id| id == g.id).unwrap());
        self.rebuild_list();
    }

    /// Drop a tab onto a group header: move it into that group, appended after
    /// the group's current last member. Mirrors drop_tab's last_active and
    /// empty-group cleanup so both drop paths leave the model consistent.
    fn drop_tab_on_group(&mut self, src: usize, group: usize) {
        let Some(si) = self.tabs.iter().position(|t| t.id == src) else {
            return;
        };
        if !self.groups.iter().any(|g| g.id == group) {
            return;
        }
        let mut tab = self.tabs.remove(si);
        let old_group = tab.group;
        tab.group = group;
        let moved_uuid = tab.uuid.clone();
        let insert_at = self
            .tabs
            .iter()
            .rposition(|t| t.group == group)
            .map_or(self.tabs.len(), |p| p + 1);
        self.tabs.insert(insert_at, tab);
        if self.active == Some(src)
            && let Some(group) = self.groups.iter_mut().find(|g| g.id == group)
        {
            group.last_active = src;
        }
        if group != old_group {
            self.session_env_refresh(&moved_uuid, group);
        }
        self.prune_empty_groups();
        self.rebuild_list();
        // No-op unless the dragged tab was the active one changing groups.
        self.sync_browser_pane();
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
            self.tab_list.append(&self.make_group_header(group));
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
        self.tab_scroller.set_visible(members.len() > 1);
        if members.len() <= 1 {
            return;
        }
        let group_css = self
            .active_tab()
            .and_then(|t| self.groups.iter().find(|g| g.id == t.group))
            .map(|g| g.css);
        let mut active_button = None;
        for tab in members {
            let active = self.active == Some(tab.id);
            // An explicit ellipsizing label is what gives the button a small
            // minimum width; Button::builder().label() builds a plain label
            // whose minimum is its full text, which is what pushed the bar
            // past the window edge.
            // No max_width_chars: the natural width stays the exact pixel
            // width of the title, so a tab is only ellipsized once the bar
            // actually runs out of room and squeezes it toward its floor.
            let label = gtk::Label::builder()
                .label(&tab.title)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .width_chars(tab_label_min_chars(tab.title.chars().count(), active))
                .build();

            let button = gtk::Button::builder().child(&label).build();
            button.add_css_class("flat");
            // No hexpand: tabs sit at their natural (title) width when the
            // bar has room, so switching tabs never reflows the whole bar.
            // When the window is too narrow they squeeze toward their
            // minimum, and past that the scroller takes over.
            button.set_tooltip_text(Some(&tab.title));
            if let Some(css) = group_css {
                button.add_css_class(css);
            }
            if active {
                button.add_css_class("tab-active");
                active_button = Some(button.clone());
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
        if let Some(button) = active_button {
            self.scroll_active_into_view(&button);
        }
    }

    /// Bring the active tab button into the scroller's visible range.
    ///
    /// Deferred to an idle callback because the buttons were only just
    /// appended: their allocations are undefined until GTK has laid the bar
    /// out, so reading them synchronously here would scroll against zeroes.
    fn scroll_active_into_view(&self, button: &gtk::Button) {
        let scroller = self.tab_scroller.clone();
        let bar = self.tab_bar.clone();
        let button = button.clone();
        gtk::glib::idle_add_local_once(move || {
            // Bounds relative to the bar, not the viewport: those are the
            // coordinates the horizontal adjustment is expressed in.
            // A later rebuild may already have torn this button out of the bar
            // — rebuild_tab_bar runs on every title change, and each run
            // schedules one of these. Scrolling to a detached widget's bounds
            // moves the bar to a garbage offset, which showed up as tabs
            // clipped against the window edge after a title update.
            if button.parent().as_ref() != Some(bar.upcast_ref::<gtk::Widget>()) {
                return;
            }
            let Some(bounds) = button.compute_bounds(&bar) else {
                return;
            };
            let (x, width) = (bounds.x() as f64, bounds.width() as f64);
            let hadj = scroller.hadjustment();
            let (value, page) = (hadj.value(), hadj.page_size());
            if x < value {
                hadj.set_value(x);
            } else if x + width > value + page {
                hadj.set_value(x + width - page);
            }
        });
    }

    /// Apply a title change by mutating the existing labels instead of
    /// rebuilding the sidebar and tab bar. Rebuilding tears down every row,
    /// button, and drag controller — per title change, at whatever rate the
    /// running program emits OSC titles — which stalls the main loop and
    /// destroys buttons mid-click (press on the old widget, release over its
    /// replacement: no click). Label mutation is cheap, and GTK's frame clock
    /// coalesces any number of them into one paint per frame.
    fn update_title(&mut self, id: usize, title: String) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if tab.title == title {
            return;
        }
        tab.title = title;
        let tab = self.tabs.iter().find(|t| t.id == id).unwrap();
        self.refresh_sidebar_label(tab);
        self.refresh_tab_bar_label(tab);
    }

    /// Update the sidebar row that displays `tab`, if any: its own row when
    /// its group is expanded, the group's representative row when collapsed
    /// (and only if `tab` is that representative — otherwise it isn't shown).
    fn refresh_sidebar_label(&self, tab: &Tab) {
        let (row_name, text) = if Some(tab.group) == self.active_group() {
            (
                tab.id.to_string(),
                row_label_text(&tab.title, tab.crashed, None),
            )
        } else {
            let members: Vec<&Tab> = self.tabs.iter().filter(|t| t.group == tab.group).collect();
            let last_active = self
                .groups
                .iter()
                .find(|g| g.id == tab.group)
                .map(|g| g.last_active);
            let representative = members
                .iter()
                .find(|t| Some(t.id) == last_active)
                .unwrap_or(&members[0]);
            if representative.id != tab.id {
                return;
            }
            (
                tab.id.to_string(),
                row_label_text(&tab.title, tab.crashed, Some(members.len())),
            )
        };
        let mut i = 0;
        while let Some(row) = self.tab_list.row_at_index(i) {
            if row.widget_name() == row_name {
                // Row structure per make_row: ListBoxRow > Box > [Label, ...].
                if let Some(label) = row
                    .child()
                    .and_then(|b| b.first_child())
                    .and_then(|w| w.downcast::<gtk::Label>().ok())
                {
                    label.set_label(&text);
                }
                return;
            }
            i += 1;
        }
    }

    /// Update `tab`'s button in the horizontal tab bar, which mirrors the
    /// active group in member order (rebuild_tab_bar appends one button per
    /// member). Hidden or foreign-group bars simply walk to nothing.
    fn refresh_tab_bar_label(&self, tab: &Tab) {
        if Some(tab.group) != self.active_group() {
            return;
        }
        let Some(index) = self
            .tabs
            .iter()
            .filter(|t| t.group == tab.group)
            .position(|t| t.id == tab.id)
        else {
            return;
        };
        let mut child = self.tab_bar.first_child();
        for _ in 0..index {
            child = child.and_then(|c| c.next_sibling());
        }
        let Some(button) = child.and_then(|c| c.downcast::<gtk::Button>().ok()) else {
            return;
        };
        button.set_tooltip_text(Some(&tab.title));
        if let Some(label) = button.child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
            let active = self.active == Some(tab.id);
            label.set_width_chars(tab_label_min_chars(tab.title.chars().count(), active));
            label.set_label(&tab.title);
        }
    }

    /// A group header row: a drag handle plus the group's name, or a muted
    /// placeholder title for unnamed groups. The whole row is a drag source
    /// (reorders the group) and a drop target (accepts a group to reorder, or
    /// a tab to move into this group).
    fn make_group_header(&self, group: &Group) -> gtk::ListBoxRow {
        let (title, named) = if group.name.is_empty() {
            // ids are 1-based, so the id doubles as a stable group number.
            (format!("Tab group {}", group.id), false)
        } else {
            (group.name.clone(), true)
        };

        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        // The symbolic handle may be absent in a sparse icon theme; fall back
        // to a menu glyph so the affordance never renders as a broken image.
        let handle_icon = if gtk::IconTheme::default().has_icon("list-drag-handle-symbolic") {
            "list-drag-handle-symbolic"
        } else {
            "open-menu-symbolic"
        };
        row_box.append(&gtk::Image::from_icon_name(handle_icon));
        row_box.append(
            &gtk::Label::builder()
                .label(&title)
                .halign(gtk::Align::Start)
                .build(),
        );

        let header = gtk::ListBoxRow::builder()
            .child(&row_box)
            .selectable(false)
            .activatable(false)
            .build();
        header.add_css_class("group-header");
        header.add_css_class(group.css);
        if !named {
            header.add_css_class("group-header-placeholder");
        }

        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        let src_group = group.id;
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("group:{src_group}").to_value(),
            ))
        });
        header.add_controller(drag);

        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let target = SidebarDropTarget::Header { group: group.id };
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        header.add_controller(drop);

        header
    }

    fn make_row(
        &self,
        tab: &Tab,
        group: &Group,
        collapsed_count: Option<usize>,
    ) -> gtk::ListBoxRow {
        let label = row_label_text(&tab.title, tab.crashed, collapsed_count);
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
                &format!("tab:{src_id}").to_value(),
            ))
        });
        row.add_controller(drag);

        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let target = SidebarDropTarget::Tab {
            tab: tab.id,
            group: group.id,
        };
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        row.add_controller(drop);

        row
    }

    /// Per-group settings: its name, and the URL a freshly launched browser in
    /// it opens on. Enter applies, Esc cancels, an empty name removes the
    /// header and an empty URL clears the default.
    ///
    /// The URL is validated as it is typed and Apply is disabled while it is
    /// unusable: `adw::AlertDialog` closes on any response, so an error raised
    /// at submit time would have nowhere left to live.
    fn show_group_settings_dialog(&self) {
        let Some(group) = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
        else {
            return;
        };

        let name_row = adw::EntryRow::builder()
            .title("Name")
            .activates_default(true)
            .build();
        name_row.set_text(&group.name);
        let url_row = adw::EntryRow::builder()
            .title("Browser default URL")
            .activates_default(true)
            .build();
        url_row.set_text(group.default_url.as_deref().unwrap_or_default());

        // Stock Adwaita style class, so the CSS provider needs nothing added.
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .build();
        error.add_css_class("error");

        let rows = adw::PreferencesGroup::new();
        rows.add(&name_row);
        rows.add(&url_row);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&rows);
        content.append(&error);

        let dialog = adw::AlertDialog::new(Some("Group Settings"), None);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("apply", "Apply");
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");

        // Live validation: the only gate on Apply. Also runs once up front, so a
        // hand-edited `state.json` opens the dialog already showing why.
        // The dialog is held weakly: this closure lives on a widget *inside* it,
        // so a strong reference would be a cycle the dialog never escapes.
        let validate = {
            let dialog = dialog.downgrade();
            let error = error.clone();
            move |text: &str| {
                let Some(dialog) = dialog.upgrade() else {
                    return;
                };
                match browser::normalize_default_url(text) {
                    Ok(_) => {
                        error.set_visible(false);
                        dialog.set_response_enabled("apply", true);
                    }
                    Err(err) => {
                        error.set_text(&err.to_string());
                        error.set_visible(true);
                        dialog.set_response_enabled("apply", false);
                    }
                }
            }
        };
        validate(&url_row.text());
        url_row.connect_changed(move |row| validate(&row.text()));

        let id = group.id;
        let input = self.input.clone();
        dialog.connect_response(Some("apply"), move |_, _| {
            // Apply is only pressable while this parses, and what it produces —
            // scheme lowercased, implied `http://` filled in — is what gets
            // stored. `None` covers both "cleared" and the unreachable error.
            let default_url = browser::normalize_default_url(&url_row.text())
                .ok()
                .flatten();
            let _ = input.send(Msg::ApplyGroupSettings {
                id,
                name: name_row.text().to_string(),
                default_url,
            });
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
            .filter(|(t, _, _)| !SHORTCUT_ALIASES.contains(t))
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

/// Which sidebar row a drop landed on. A tab row can host a tab (reorder) or a
/// group (reorder groups); a header can host a tab (move into the group) or a
/// group (reorder groups).
#[derive(Debug, Clone, Copy)]
enum SidebarDropTarget {
    Tab { tab: usize, group: usize },
    Header { group: usize },
}

/// Parse a namespaced sidebar DnD payload ("tab:<id>" / "group:<id>") and send
/// the message appropriate to where it was dropped. Returns whether the drop
/// was accepted. A missing prefix or unparsable id is rejected.
fn dispatch_sidebar_drop(
    input: &relm4::Sender<Msg>,
    value: &gtk::glib::Value,
    target: SidebarDropTarget,
) -> bool {
    let Ok(payload) = value.get::<String>() else {
        return false;
    };
    let Some((kind, id)) = payload.split_once(':') else {
        return false;
    };
    let Ok(src) = id.parse::<usize>() else {
        return false;
    };
    let msg = match (kind, target) {
        ("tab", SidebarDropTarget::Tab { tab, .. }) => Msg::DropTab { src, dest: tab },
        ("tab", SidebarDropTarget::Header { group }) => Msg::DropTabOnGroup { src, group },
        ("group", SidebarDropTarget::Tab { group, .. } | SidebarDropTarget::Header { group }) => {
            Msg::DropGroup { src, dest: group }
        }
        _ => return false,
    };
    let _ = input.send(msg);
    true
}

/// Reorder a list of group ids by removing `src` and reinserting it immediately
/// before `dest`. A no-op when src == dest or either id is absent. Pure so the
/// reorder can be tested without GTK widgets.
fn reorder_groups(order: &mut Vec<usize>, src: usize, dest: usize) {
    if src == dest {
        return;
    }
    let Some(si) = order.iter().position(|&id| id == src) else {
        return;
    };
    let id = order.remove(si);
    let Some(di) = order.iter().position(|&d| d == dest) else {
        order.insert(si, id); // dest vanished; leave order unchanged
        return;
    };
    order.insert(di, id);
}

/// Should a group be persisted as having its browser open? True while it has a
/// live `Browser`, and also while it is still queued for restore — restore is
/// sequential and idle-driven, so several groups are legitimately "open but not
/// yet spawned" at once, and a save in that window must not forget them.
fn browser_open_desired(has_browser: bool, pending_restore: &[usize], id: usize) -> bool {
    has_browser || pending_restore.contains(&id)
}

/// Can a captured frame back a `gdk::MemoryTexture`? Pure.
///
/// GDK reads `stride * height` bytes out of the buffer it is handed and trusts
/// the geometry, so a degenerate or short frame is a failure to report rather
/// than something to pass on to it.
fn frame_backs_texture(width: u32, height: u32, stride: usize, len: usize) -> bool {
    let needed = stride.saturating_mul(height as usize);
    width > 0 && height > 0 && stride > 0 && needed > 0 && len >= needed
}

/// Uuids whose profile directory nobody will dispose of when the groups not in
/// `live` are dropped. Pure.
///
/// A group that still holds a `Browser` needs no entry here: dropping the
/// `Browser` removes the profile itself. A group whose browser already died
/// (profile deliberately kept) has nothing left to run that cleanup, so its
/// profile is named here instead.
fn reclaimable_profiles<'a, I>(groups: I, live: &HashSet<usize>) -> Vec<String>
where
    I: IntoIterator<Item = (usize, &'a str, bool)>,
{
    groups
        .into_iter()
        .filter(|(id, _, has_browser)| !has_browser && !live.contains(id))
        .map(|(_, uuid, _)| uuid.to_string())
        .collect()
}

/// May the stale-profile sweep run? Only when the group uuids in the loaded
/// state are known to be on disk. Pure.
///
/// The sweep deletes every profile directory whose name is not a live group
/// uuid. That is only sound while the uuids are stable across restarts. When
/// `state::load` had to backfill uuids and writing them back failed, the next
/// start mints *different* ones — so today's "live" set is provably worthless
/// as a liveness oracle, and every existing profile would look stale. Standing
/// down leaves at most some genuinely stale directories on disk, which the next
/// successful start sweeps up; the alternative destroys live sessions.
fn profile_sweep_allowed(uuids_unpersisted: bool) -> bool {
    !uuids_unpersisted
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

/// Text of a sidebar row for a tab: the crash-marked title when the row is
/// one tab of the expanded group, or "title (n)" when the row stands in for
/// a collapsed group of n tabs. Shared by row construction and the in-place
/// title update so the two can never drift apart.
fn row_label_text(title: &str, crashed: Option<i32>, collapsed_count: Option<usize>) -> String {
    match collapsed_count {
        Some(n) => format!("{title} ({n})"),
        None => crashed_tab_label(title, crashed),
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

/// Body text of the linger (logout survival) warning dialog. Ordered per the
/// spec: what enabling adds comes first, then the honest, non-dramatized
/// downsides (background footprint, unattended processes on shared machines,
/// persisting state), closing with reversibility via `disable-linger`.
fn linger_warning_body() -> String {
    "Enabling lingering keeps your shells running after you log out and back \
     in, not only when kabelsalat is closed, crashes, or is upgraded.\n\n\
     In exchange:\n\
     \u{2022} A small, permanent background footprint: your user service \
     manager and enabled user services keep running while you are logged out.\n\
     \u{2022} \u{201c}Logged out\u{201d} no longer means nothing of yours is \
     running \u{2014} long-running processes and agents keep going unattended, \
     worth considering on a shared machine.\n\
     \u{2022} State that a fresh login used to clear can persist between \
     sessions.\n\n\
     You can turn this off again at any time with `loginctl disable-linger`."
        .to_string()
}

/// Minimum width request (`width_chars`) for a tab button's label.
///
/// The minimum is all we set: the label's natural width stays its exact
/// title width, so every tab renders in full whenever the bar has room.
/// When it does not, GTK's shortage distribution squeezes the widest tabs
/// first, each down to this floor, and past that the bar scrolls. The active
/// tab keeps a higher floor so it stays readable under pressure.
fn tab_label_min_chars(title_chars: usize, active: bool) -> i32 {
    let floor = if active {
        ACTIVE_TAB_MIN_CHARS
    } else {
        TAB_MIN_CHARS
    };
    (title_chars as i32).min(floor)
}

fn select_help_body() -> &'static str {
    "Drag with Shift held to select text with the mouse.\n\n\
     Shells run inside tmux, and tmux has to claim the mouse so the wheel can \
     scroll the scrollback instead of typing arrow keys into your shell. \
     Mouse reporting is all-or-nothing, so buttons go to tmux too \u{2014} \
     holding Shift is the terminal's standard way to take them back for \
     selection. Other terminals behave the same way under tmux.\n\n\
     \u{2022} Shift+drag \u{2014} select text\n\
     \u{2022} Wheel \u{2014} scroll the scrollback (Escape or q to leave)"
}

/// Detect a drag that the user probably meant as a text selection but which
/// tmux will swallow, because Shift was not held.
///
/// Split out from the gesture wiring so it can be tested without a display:
/// GTK gestures need a real GDK surface, this decision does not.
fn should_hint(distance: f64, mods: gtk::gdk::ModifierType, already_fired: bool) -> bool {
    !already_fired
        && distance >= DRAG_THRESHOLD_PX
        && !mods.contains(gtk::gdk::ModifierType::SHIFT_MASK)
}

/// Watch for Shift-less drags on `terminal` and raise the selection hint.
///
/// The gesture sits in the capture phase so it sees the drag on the way down
/// the widget tree, and never claims the sequence, so VTE (and through it
/// tmux) still receives every event untouched.
fn attach_drag_hint(terminal: &Terminal, sender: &ComponentSender<App>) {
    let drag = gtk::GestureDrag::new();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);

    // One-shot per drag: a single gesture emits drag_update on every motion
    // event, and the hint should appear once, not once per pixel.
    let fired = std::rc::Rc::new(std::cell::Cell::new(false));
    drag.connect_drag_update({
        let sender = sender.clone();
        let fired = fired.clone();
        move |drag, off_x, off_y| {
            let distance = (off_x * off_x + off_y * off_y).sqrt();
            if should_hint(distance, drag.current_event_state(), fired.get()) {
                fired.set(true);
                let _ = sender.input_sender().send(Msg::BareDragHint);
            }
        }
    });
    drag.connect_drag_end(move |_, _, _| fired.set(false));

    terminal.add_controller(drag);
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
/// `command`, when set, replaces the shell in either path.
fn spawn_backing(
    terminal: &Terminal,
    uuid: &str,
    tmux: Option<&TmuxCtl>,
    cwd: Option<&Path>,
    command: Option<&[String]>,
    env: &[(&str, &str)],
) {
    let Some(ctl) = tmux else {
        spawn_shell(terminal, cwd, command);
        return;
    };
    let argv = ctl.spawn_argv(uuid, cwd, command, env);
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

fn spawn_shell(terminal: &Terminal, cwd: Option<&Path>, command: Option<&[String]>) {
    // Without tmux there is no re-joining to worry about: VTE takes argv
    // directly, so the command's word boundaries survive exactly.
    let argv: Vec<String> = match command {
        Some(command) if !command.is_empty() => command.to_vec(),
        _ => vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())],
    };
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let working_dir = cwd.map(|p| p.to_string_lossy().into_owned());
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        working_dir.as_deref(),
        &refs,
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

    #[test]
    fn live_browser_is_persisted_as_open() {
        assert!(browser_open_desired(true, &[], 1));
    }

    #[test]
    fn group_queued_for_restore_is_persisted_as_open() {
        // The regression: a save inside the sequential restore window must
        // not write browser_open=false for groups not spawned yet.
        assert!(browser_open_desired(false, &[2, 3], 3));
        assert!(browser_open_desired(false, &[2, 3], 2));
    }

    #[test]
    fn group_without_browser_and_not_queued_is_persisted_as_closed() {
        assert!(!browser_open_desired(false, &[2, 3], 4));
        assert!(!browser_open_desired(false, &[], 1));
    }

    // --- captured frames -------------------------------------------------

    #[test]
    fn a_full_frame_backs_a_texture() {
        assert!(frame_backs_texture(800, 600, 3200, 3200 * 600));
        // Padded rows and an over-long buffer are both fine.
        assert!(frame_backs_texture(800, 600, 4096, 4096 * 600 + 17));
    }

    #[test]
    fn a_degenerate_or_short_frame_backs_nothing() {
        // Nothing to show…
        assert!(!frame_backs_texture(0, 600, 3200, 3200 * 600));
        assert!(!frame_backs_texture(800, 0, 3200, 0));
        assert!(!frame_backs_texture(800, 600, 0, 3200 * 600));
        // …and a buffer GDK would read past the end of.
        assert!(!frame_backs_texture(800, 600, 3200, 3200 * 600 - 1));
        assert!(!frame_backs_texture(800, 600, 3200, 0));
        // No overflow panic on absurd geometry, just a refusal.
        assert!(!frame_backs_texture(u32::MAX, u32::MAX, usize::MAX, 4));
    }

    // --- profile reclamation on group deletion ---------------------------

    fn live_ids(ids: &[usize]) -> HashSet<usize> {
        ids.iter().copied().collect()
    }

    #[test]
    fn a_deleted_group_whose_browser_already_died_gets_its_profile_reclaimed() {
        // The regression: after BrowserDied the profile is kept on purpose,
        // but then no Browser remains to dispose of it when the group goes.
        let groups = [(1, "uuid-1", false), (2, "uuid-2", false)];
        assert_eq!(
            reclaimable_profiles(groups.iter().copied(), &live_ids(&[1])),
            vec!["uuid-2".to_string()]
        );
    }

    #[test]
    fn a_deleted_group_with_a_live_browser_is_left_to_its_browser() {
        // Dropping the `Browser` removes the profile; queueing it here too
        // would delete a path twice.
        let groups = [(2, "uuid-2", true)];
        assert!(reclaimable_profiles(groups.iter().copied(), &live_ids(&[])).is_empty());
    }

    #[test]
    fn surviving_groups_never_lose_their_profile() {
        let groups = [(1, "uuid-1", false), (2, "uuid-2", true)];
        assert!(reclaimable_profiles(groups.iter().copied(), &live_ids(&[1, 2])).is_empty());
    }

    // --- profile keys are uuids, never reusable ids -----------------------

    #[test]
    fn profile_paths_are_keyed_by_uuid_so_a_reused_id_cannot_collide() {
        // app.rs rebuilds next_group_id as max(id) + 1, so deleting the
        // highest group makes the next one reuse that id. Two such groups must
        // still get different profile directories.
        let old = state::SavedGroup::new(3, "old".to_string(), 0);
        let new = state::SavedGroup::new(3, "new".to_string(), 0);
        assert_eq!(old.id, new.id);
        assert_ne!(old.uuid, new.uuid);
        let dir = std::path::Path::new("/state");
        assert_ne!(
            browser::profile_dir(dir, &old.uuid),
            browser::profile_dir(dir, &new.uuid)
        );
    }

    #[test]
    fn a_group_uuid_survives_the_save_load_roundtrip_into_the_profile_path() {
        let saved = state::SavedGroup::new(1, "g".to_string(), 0);
        let restored_uuid = saved.uuid.clone();
        let dir = std::path::Path::new("/state");
        assert_eq!(
            browser::profile_dir(dir, &restored_uuid),
            browser::profile_dir(dir, &saved.uuid)
        );
        // And the sweep considers exactly that key live.
        let live: HashSet<String> = std::iter::once(restored_uuid.clone()).collect();
        assert!(!browser::is_stale_profile(&restored_uuid, &live));
        assert!(browser::is_stale_profile("1", &live));
    }

    fn session(uuid: &str) -> SessionInfo {
        SessionInfo {
            uuid: uuid.to_string(),
            pane_dead: false,
            dead_status: None,
        }
    }

    // --- tab bar sizing --------------------------------------------------

    #[test]
    fn tab_minimum_is_a_small_floor() {
        // Regression: pinning minimum to the full width made the bar's own
        // minimum 474px, so any narrower window overflowed and clipped tabs.
        // The minimum must stay small so the bar can shrink at all.
        assert_eq!(tab_label_min_chars(200, true), ACTIVE_TAB_MIN_CHARS);
        assert_eq!(tab_label_min_chars(200, false), TAB_MIN_CHARS);
    }

    #[test]
    fn short_title_does_not_inflate_the_minimum() {
        // A 3-char title must not request an 8-char floor.
        assert_eq!(tab_label_min_chars(3, true), 3);
        assert_eq!(tab_label_min_chars(2, false), 2);
    }

    #[test]
    fn empty_title_does_not_underflow() {
        assert_eq!(tab_label_min_chars(0, true), 0);
    }

    // --- Shift-select hint decision --------------------------------------

    use gtk::gdk::ModifierType;

    #[test]
    fn hints_on_bare_drag_past_threshold() {
        assert!(should_hint(20.0, ModifierType::empty(), false));
    }

    #[test]
    fn no_hint_when_shift_held() {
        // Shift+drag is a *working* selection — hinting would be wrong.
        assert!(!should_hint(20.0, ModifierType::SHIFT_MASK, false));
        // Shift alongside other modifiers still counts as held.
        assert!(!should_hint(
            20.0,
            ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK,
            false
        ));
    }

    #[test]
    fn no_hint_below_drag_threshold() {
        // A click with a shaky hand is not an attempted selection.
        assert!(!should_hint(
            DRAG_THRESHOLD_PX - 0.1,
            ModifierType::empty(),
            false
        ));
    }

    #[test]
    fn no_hint_twice_within_one_drag() {
        // drag_update fires per motion event; the hint must fire once.
        assert!(!should_hint(500.0, ModifierType::empty(), true));
    }

    #[test]
    fn hint_fires_exactly_at_threshold() {
        assert!(should_hint(DRAG_THRESHOLD_PX, ModifierType::empty(), false));
    }

    // --- group reorder (drag-and-drop) ----------------------------------

    #[test]
    fn reorder_moves_src_before_dest() {
        let mut order = vec![1, 2, 3, 4];
        reorder_groups(&mut order, 4, 2);
        assert_eq!(order, [1, 4, 2, 3]);
    }

    #[test]
    fn reorder_earlier_before_later() {
        let mut order = vec![1, 2, 3, 4];
        reorder_groups(&mut order, 1, 3);
        assert_eq!(order, [2, 1, 3, 4]);
    }

    #[test]
    fn reorder_same_is_noop() {
        let mut order = vec![1, 2, 3];
        reorder_groups(&mut order, 2, 2);
        assert_eq!(order, [1, 2, 3]);
    }

    #[test]
    fn reorder_missing_id_is_noop() {
        let mut order = vec![1, 2, 3];
        reorder_groups(&mut order, 9, 2);
        assert_eq!(order, [1, 2, 3]);
        reorder_groups(&mut order, 2, 9);
        assert_eq!(order, [1, 2, 3]);
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

    // --- sidebar row label text ------------------------------------------

    #[test]
    fn expanded_row_shows_title_with_crash_marker() {
        assert_eq!(row_label_text("bash", Some(3), None), "bash [exit 3]");
        assert_eq!(row_label_text("bash", None, None), "bash");
    }

    #[test]
    fn collapsed_row_shows_title_with_count() {
        // A collapsed group's row shows its member count, never the crash
        // marker (the representative stands in for the whole group).
        assert_eq!(row_label_text("bash", Some(3), Some(4)), "bash (4)");
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

    // --- linger-warning dialog body -------------------------------------

    #[test]
    fn linger_body_puts_enables_before_downsides() {
        let body = linger_warning_body();
        let enables = body
            .find("keeps your shells running after you log out")
            .expect("enables clause present");
        let downside = body
            .find("permanent background footprint")
            .expect("downside clause present");
        assert!(enables < downside, "enables must precede downsides");
    }

    #[test]
    fn linger_body_mentions_logout_survival() {
        let body = linger_warning_body();
        assert!(body.contains("log out"));
        assert!(body.contains("crashes"));
    }

    #[test]
    fn linger_body_mentions_downsides_and_shared_machine() {
        let body = linger_warning_body();
        assert!(body.contains("background footprint"));
        assert!(body.contains("shared machine"));
        assert!(body.contains("unattended"));
    }

    #[test]
    fn linger_body_mentions_reversibility_via_disable_linger() {
        let body = linger_warning_body();
        assert!(body.contains("disable-linger"));
    }

    #[test]
    fn sweep_is_allowed_when_uuids_are_known_to_be_on_disk() {
        assert!(profile_sweep_allowed(false));
    }

    #[test]
    fn sweep_stands_down_when_backfilled_uuids_were_not_persisted() {
        // The regression: a failed state write used to leave freshly minted
        // uuids in memory only, and the sweep then deleted every profile
        // directory as "stale" — destroying live browser sessions.
        assert!(!profile_sweep_allowed(true));
    }

    #[test]
    fn a_backfilled_but_successfully_persisted_load_still_sweeps() {
        // Backfill alone must not disable cleanup forever: once the write
        // lands, the uuids are stable and the sweep is sound again.
        let dir =
            std::env::temp_dir().join(format!("kabelsalat-sweep-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            br#"{"groups":[{"id":0,"name":"","palette":0}],"tabs":[],"active":null,"sidebar_visible":true}"#,
        )
        .unwrap();

        let loaded = state::load_detailed(&path);
        assert!(loaded.uuids_backfilled);
        assert!(!profile_sweep_allowed(loaded.uuids_backfilled));

        // This is what `restore_or_fresh` does with the backfilled state.
        state::save(&loaded.state, &path).unwrap();
        let again = state::load_detailed(&path);
        assert!(!again.uuids_backfilled);
        assert!(profile_sweep_allowed(again.uuids_backfilled));
        assert_eq!(again.state.groups[0].uuid, loaded.state.groups[0].uuid);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
