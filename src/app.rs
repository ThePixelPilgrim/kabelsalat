use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relm4::adw;
use relm4::adw::prelude::{AdwDialogExt, AlertDialogExt, PreferencesGroupExt};
use relm4::gtk;
use relm4::gtk::gdk::RGBA;
use relm4::gtk::gio;
use relm4::gtk::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt, SimpleComponent};
use vte4::{PtyFlags, Terminal, TerminalExt, TerminalExtManual};

use crate::android::{self, Android};
use crate::autostart;
use crate::browser::{self, Browser, CapturedFrame, ProfileDisposition};
use crate::claude;
use crate::cli::{ANDROID_ENV_KEYS, CDP_ENV_KEYS};
use crate::control;
use crate::overview::canvas::{Callbacks, EmptyState, OverviewView, TabChip, Unmapped};
use crate::overview::issues::{self, Issue, TaggedTab};
use crate::overview::layout::{self, Layout, Metrics};
use crate::overview::model::{Graph, Source, scan_root};
use crate::overview::tagger::{self, ENV_LINKS, ENV_TAG_MARK, NodeSummary, Tags, Watermark};
use crate::remote::{self, HostState, RemoteError};
use crate::remote_worker::{self, RemoteEvent, RemoteWorker, SshRunner};
use crate::state::{
    self, BrowserKeyStep, ClaudeSession, PaneKind, SavedGroup, SavedState, SavedTab, SidebarOrder,
};
use crate::tmuxctl::{self, LingerStatus, SessionInfo, TmuxAvailability, TmuxCtl, TmuxError};

pub const GROUP_PALETTE: [&str; 6] = [
    "group-c0", "group-c1", "group-c2", "group-c3", "group-c4", "group-c5",
];

const SHORTCUTS: &[(&str, &str, Msg)] = &[
    ("<Control><Shift>t", "New tab in active group", Msg::NewTab),
    ("<Control><Shift>n", "New group", Msg::NewGroup),
    (
        "<Control><Shift>h",
        "New remote group",
        Msg::NewRemoteGroupDialog,
    ),
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
    (
        "<Alt>2",
        "Browser pane: open, bring to front, or hide",
        Msg::ToggleBrowser,
    ),
    (
        "<Alt><Shift>2",
        "Browser pane: open, bring to front, or hide",
        Msg::ToggleBrowser,
    ),
    (
        "<Alt>at",
        "Browser pane: open, bring to front, or hide",
        Msg::ToggleBrowser,
    ),
    (
        "<Alt>quotedbl",
        "Browser pane: open, bring to front, or hide",
        Msg::ToggleBrowser,
    ),
    (
        "<Alt>3",
        "Android pane: open or bring to front",
        Msg::ToggleAndroid,
    ),
    (
        "<Alt><Shift>3",
        "Android pane: open or bring to front",
        Msg::ToggleAndroid,
    ),
    // Shift+3 is `#` on US layouts and `§` on German ones.
    (
        "<Alt>numbersign",
        "Android pane: open or bring to front",
        Msg::ToggleAndroid,
    ),
    (
        "<Alt>section",
        "Android pane: open or bring to front",
        Msg::ToggleAndroid,
    ),
    // Shift is what keeps these off the pty: plain Ctrl-V still reaches the
    // child as 0x16, so `claude`, vim and readline keep their own handling.
    ("<Control><Shift>v", "Paste into the terminal", Msg::Paste),
    // One key, two jobs that never overlap: with text selected it copies,
    // otherwise it opens a claude tab.
    (
        "<Control><Shift>c",
        "Copy the selection, or open a claude tab",
        Msg::Copy,
    ),
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
    "<Alt><Shift>3",
    "<Alt>numbersign",
    "<Alt>section",
];

/// How often the hosted Chromium processes are reaped for exit.
const BROWSER_POLL_SECS: u32 = 2;

/// Pages of the mode stack: the terminal area, or the overview.
const MODE_TERMINALS: &str = "terminals";
const MODE_OVERVIEW: &str = "overview";

/// How often the tab age prefixes are recomputed. The display has minute
/// granularity, so a 30 s tick shows a stale "now" for at most 89 s.
const AGE_TICK_SECS: u32 = 30;

/// How often the tabs are matched against the claude processes running in
/// them. Cheap (a few small files, one `list-panes`), so often enough that a
/// crash shortly after a claude starts still knows what to resume.
const CLAUDE_TICK_SECS: u32 = 5;

/// How long a live host rests between two discoveries: each is a round trip
/// on its serial worker queue, so it is paced below the local tick.
const REMOTE_CLAUDE_SECS: u64 = 15;
/// How long after a tab's first output — and after every tab activation —
/// the activity sources (`contents-changed`, title writes) stay ignored.
/// Attaching a tmux session repaints the whole screen; without this window
/// every restored tab would be stamped "now" and the persisted age seed
/// (applied right after `add_tab`) would be lost. Activating a tab can
/// likewise make the program in the pane repaint (focus events), and that
/// echo of the switch must never reorder an Activity-sorted sidebar. See
/// `ActivityArm`.
const ACTIVITY_SETTLE_MS: u64 = 750;

/// How long after an activity stamp the sidebar re-sorts. The stamps are
/// written by VTE callbacks outside the message loop (per output chunk), so
/// they are coalesced into one `Msg::Resort` per window rather than a
/// rebuild each.
const ACTIVITY_RESORT_MS: u64 = 250;

/// A frozen navigation order — group id plus that group's tab ids in
/// display order, groups in sidebar order — and the expiry timeout that ends
/// the burst.
type NavBurst = Option<(Vec<(usize, Vec<usize>)>, gtk::glib::SourceId)>;

/// How long a burst of Ctrl-Page_Up/Down keeps navigating the order it
/// started with. The live order can reshuffle between presses (an idle tab
/// receives an update and, in Activity sort, jumps to the front); without a
/// frozen snapshot the walk would jump with it. Any other way of landing on
/// a tab (click, jump, close) ends the burst early via the staleness check
/// in `navigate`. The sidebar holds still for the same window (see
/// `resort_if_stale`), so what the keys walk is what the list shows.
const NAV_BURST_MS: u64 = 3000;

/// CDP discovery: poll the profile's DevToolsActivePort file this often…
const CDP_POLL_MS: u64 = 100;
/// …and give up after this many attempts (10 s total).
const CDP_POLL_TRIES: u32 = 100;

/// Environment keys published into each tab's tmux session (see
/// docs/superpowers/specs/2026-07-30-cdp-endpoint-design.md); the pair
/// constants and the builders live in `cli.rs`, where they are tested.
const ENV_GROUP: &str = crate::cli::ENV_GROUP;

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

/// Toast for a tab move across hosts, the drop-time authority's refusal.
const CROSS_HOST_TOAST: &str = "Can't move a tab between hosts";

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

/// How long a new remote tab waits for the active tab's remote directory
/// before starting in the remote home instead.
const REMOTE_CWD_WAIT: Duration = Duration::from_millis(300);

/// A remote client that exits sooner than this after its spawn, while its
/// session lives on, is not reattached automatically — that would loop.
const REATTACH_GUARD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq)]
enum PickerMode {
    Move,
    Jump,
}

/// Which form the group settings dialog shows: the one that creates a remote
/// group, or the active group's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SettingsMode {
    Create,
    Edit,
}

/// What a remote tab spawns once its host is live: the directory and command
/// it was created with. Consumed by the first spawn; a later reattach needs
/// neither (`new-session -A` ignores `-c` for an existing session).
#[derive(Debug, Default)]
struct PendingSpawn {
    cwd: Option<PathBuf>,
    command: Option<Vec<String>>,
}

/// The page a remote tab shows instead of its terminal while its host is
/// connecting or disconnected, or while the tab itself is detached.
struct HostPage {
    page: adw::StatusPage,
    spinner: gtk::Spinner,
    reconnect: gtk::Button,
}

impl HostPage {
    fn new(host: &str, input: &relm4::Sender<Msg>) -> Self {
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .build();
        let reconnect = gtk::Button::builder()
            .label("Reconnect")
            .halign(gtk::Align::Center)
            .build();
        reconnect.add_css_class("pill");
        reconnect.add_css_class("suggested-action");
        reconnect.connect_clicked({
            let input = input.clone();
            let host = host.to_string();
            move |_| {
                let _ = input.send(Msg::Reconnect(host.clone()));
            }
        });
        let child = gtk::Box::new(gtk::Orientation::Vertical, 12);
        child.append(&spinner);
        child.append(&reconnect);
        let page = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .child(&child)
            .build();
        Self {
            page,
            spinner,
            reconnect,
        }
    }

    fn show(&self, title: &str, description: &str, connecting: bool, reconnect: bool) {
        self.page.set_title(title);
        // The description is Pango markup, and ssh's stderr can hold '<'/'&'.
        let markup = gtk::glib::markup_escape_text(description);
        self.page
            .set_description(Some(markup.as_str()).filter(|text| !text.is_empty()));
        self.spinner.set_visible(connecting);
        self.spinner.set_spinning(connecting);
        self.reconnect.set_visible(reconnect);
    }
}

/// One remote host as the app sees it.
struct RemoteHost {
    state: HostState,
    /// Present once a connect was attempted with a usable ssh.
    link: Option<HostLink>,
    /// Background retries of a disconnected host; runtime only.
    retry: Retry,
    /// Claude discovery on a live host; runtime only.
    discovery: Discovery,
}

/// Where a live host stands with its claude discoveries.
#[derive(Default)]
struct Discovery {
    /// One is on the worker's queue; its answer (or the host going down)
    /// clears this. Never two in flight: a slow host must not pile them up.
    running: bool,
    /// Not before this; set when the last answer arrived.
    next: Option<Instant>,
}

/// Where a disconnected host stands with its background retries.
#[derive(Default)]
struct Retry {
    /// Failed retries since the host was last live or Reconnect was clicked.
    failures: u32,
    /// When the next one is due; set by the first tick that finds it unset.
    due: Option<Instant>,
    /// One is running on the worker.
    running: bool,
}

struct HostLink {
    /// Builds the tabs' `ssh … tmux new-session -A …` argv.
    ctl: TmuxCtl,
    worker: RemoteWorker,
}

pub struct Tab {
    id: usize,
    uuid: String, // stable across restarts; names the backing tmux session
    group: usize,
    title: String,
    crashed: Option<i32>, // shell exit code when the tab crashed
    terminal: Terminal,
    /// Wall-clock stamp of the last *meaningful* activity on this tab: an
    /// Esc/Enter press, or a title that actually changed. Persisted in
    /// `SavedTab::last_activity` — tmux has nothing per-pane to offer here
    /// (every tmux timestamp is output-derived, so an idle-but-repainting
    /// agent would stay perpetually fresh), which makes the state file the
    /// only truthful seed across restarts. Shared with the key controller,
    /// which fires per keystroke and so must not go through the relm4
    /// message loop.
    last_activity: Rc<Cell<SystemTime>>,
    /// Gate on the output/title activity sources — see `ActivityArm`.
    /// Shared with the callbacks that stamp `last_activity` so a repaint
    /// that is merely the echo of an attach or a switch cannot reorder an
    /// Activity-sorted sidebar.
    armed: Rc<ActivityArm>,
    /// Age prefix currently rendered in the labels; the tick only touches a
    /// label when this changes.
    age_shown: String,
    /// What the stack shows for this tab: its terminal ("terminal") or —
    /// remote tabs only — the host page ("status").
    view: gtk::Stack,
    /// Remote tabs only; `None` for local tabs, which always show the
    /// terminal.
    status: Option<HostPage>,
    /// Whether the terminal runs this tab's tmux client. Always true for a
    /// local tab. A remote tab is detached until its host is live, and again
    /// after its ssh client exits.
    attached: bool,
    /// Remote tabs only: the first spawn's directory and command, kept until
    /// the host is live.
    pending: Option<PendingSpawn>,
    /// When a remote tab's client was last spawned; the reattach guard
    /// compares against it. `None` for local tabs.
    spawned_at: Option<Instant>,
    /// The claude session last seen in this tab (see `claude.rs`): what a
    /// respawn of the tab brings back instead of a shell.
    claude: Option<ClaudeSession>,
    /// What the tagger last said this tab works on (overview mode). Mirrored
    /// into the tab's tmux session as `KABELSALAT_LINKS`, never persisted
    /// here; without tmux it lives only in memory.
    tags: Option<Tags>,
    /// Where the last tagger run left off (`KABELSALAT_TAG_MARK`).
    tag_mark: Option<Watermark>,
    /// When the tagger last ran for this tab; drives the per-tab rate limit.
    last_tag_run: Option<Instant>,
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
    /// The Android pane, in at most one group at a time (one Waydroid per
    /// machine). Dropping it stops the session and closes the pane.
    android: Option<Android>,
    /// Whether the pane area is shown rather than hidden. Only meaningful
    /// while a pane is open; hidden panes keep running.
    panes_visible: bool,
    /// Which pane the area shows on top when it holds more than one. Runtime
    /// only: a restart brings the browser back in front.
    front_pane: PaneKind,
    /// The tab bar and tab view holding this group's panes; parented into
    /// `browser_paned` only while this is the active group and shows a pane.
    panes: PaneHost,
    /// Divider position of the terminal/browser split, in pixels.
    browser_split: f64,
    /// Where a freshly launched browser for this group starts, already
    /// normalized. `None` = whatever Chromium opens on its own. Editing it never
    /// touches a running browser: it is a launch argument, and only a launch
    /// into a *fresh* profile uses it.
    default_url: Option<String>,
    /// The ssh destination this group's tabs run on; `None` = this computer.
    /// Fixed for the group's lifetime: tabs cannot move between hosts.
    host: Option<String>,
    /// The directory whose node files the overview draws; `None` = the
    /// overview shows its empty state. Set by `kabelsalat overview root`.
    overview_root: Option<PathBuf>,
    /// Whether the group shows the overview instead of its terminals.
    overview_mode: bool,
    /// The loaded overview: graph, layout, monitors and issues. Runtime
    /// only, built lazily the first time the group's overview is needed and
    /// dropped (monitors included) when the root is cleared.
    overview: Option<OverviewData>,
}

/// A group's overview as loaded from its root. `monitors` must stay stored:
/// dropping a `gio::FileMonitor` stops it.
struct OverviewData {
    graph: Rc<Graph>,
    layout: Rc<Layout>,
    /// Why the root could not be read (the empty state's text); the graph is
    /// empty then.
    error: Option<String>,
    /// One directory monitor per directory below the root, by path.
    monitors: HashMap<PathBuf, gio::FileMonitor>,
    /// The current data issues, as the tray and `overview issues` show them.
    issues: Vec<Issue>,
    /// Bumped on every graph change; the view is re-fed when it differs from
    /// what it last got (see `App::fed_overview`).
    generation: u64,
}

impl Group {
    /// Does the pane area hold at least one pane?
    fn has_panes(&self) -> bool {
        !self.panes.is_empty()
    }

    /// The widget of the pane in front, if that pane is open.
    fn front_widget(&self) -> Option<gtk::Widget> {
        self.panes.page(self.front_pane).map(|page| page.child())
    }

    /// Add `kind`'s page holding `widget` and pick the front pane: an
    /// interactive open puts it in front, a quiet one (CLI, restore) only
    /// when no other pane is open (see `state::front_after_open`). Leaves
    /// the area's visibility to the caller; returns whether another pane
    /// was already open, which that decision needs.
    fn add_pane(&mut self, kind: PaneKind, widget: &gtk::Widget, quiet: bool) -> bool {
        let other_open = self.has_panes();
        self.panes.add(kind, widget);
        self.front_pane = state::front_after_open(self.front_pane, kind, other_open, quiet);
        other_open
    }
}

/// A group's pane area: an `adw::TabBar` over an `adw::TabView`, one page
/// per open pane. Owns no pane: the `Browser`/`Android` on the group do, and
/// their widgets are only parented here.
struct PaneHost {
    root: gtk::Box,
    view: adw::TabView,
    /// The open pages by kind; the view itself has no notion of kinds.
    pages: RefCell<Vec<(PaneKind, adw::TabPage)>>,
    /// Set while the app adds, removes or selects pages itself, so the view's
    /// signals do not echo those changes back as messages.
    syncing: Rc<Cell<bool>>,
}

impl PaneHost {
    /// `group` is the owning group's id; the host's selection messages carry
    /// it, so a notify from a host that is not attached is ignored.
    fn new(input: &relm4::Sender<Msg>, group: usize) -> Self {
        let view = adw::TabView::new();
        // Alt+digits and Ctrl+PgUp/PgDn belong to the window's shortcuts.
        view.set_shortcuts(adw::TabViewShortcuts::NONE);
        view.set_hexpand(true);
        view.set_vexpand(true);
        let bar = adw::TabBar::new();
        bar.set_view(Some(&view));
        // One pane still gets its tab: it says what the pane is and carries
        // the close button.
        bar.set_autohide(false);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&bar);
        root.append(&view);

        let syncing = Rc::new(Cell::new(false));
        view.connect_close_page({
            let syncing = syncing.clone();
            let input = input.clone();
            move |view, page| {
                if syncing.get() {
                    // The app's own removal: let the default handler finish it.
                    return gtk::glib::Propagation::Proceed;
                }
                // A click on a tab's close button. Refuse it here; the close
                // path tears the pane down and removes the page itself.
                view.close_page_finish(page, false);
                match PaneKind::from_widget_name(page.child().widget_name().as_str()) {
                    Some(PaneKind::Browser) => {
                        let _ = input.send(Msg::CloseBrowser);
                    }
                    Some(PaneKind::Android) => {
                        let _ = input.send(Msg::StopAndroid);
                    }
                    None => {}
                }
                gtk::glib::Propagation::Stop
            }
        });
        view.connect_selected_page_notify({
            let syncing = syncing.clone();
            let input = input.clone();
            move |view| {
                if syncing.get() {
                    return;
                }
                if let Some(kind) = view.selected_page().and_then(|page| {
                    PaneKind::from_widget_name(page.child().widget_name().as_str())
                }) {
                    let _ = input.send(Msg::PaneSelected(group, kind));
                }
            }
        });

        Self {
            root,
            view,
            pages: RefCell::new(Vec::new()),
            syncing,
        }
    }

    /// Add a page for `kind` holding `widget`. A kind that already has a page
    /// is left alone.
    fn add(&self, kind: PaneKind, widget: &gtk::Widget) {
        if self.page(kind).is_some() {
            return;
        }
        widget.set_widget_name(kind.widget_name());
        self.syncing.set(true);
        let page = self.view.append(widget);
        page.set_title(kind.title());
        self.syncing.set(false);
        self.pages.borrow_mut().push((kind, page));
    }

    /// Remove `kind`'s page; its widget is unparented, not destroyed — the
    /// pane that owns it decides that.
    fn remove(&self, kind: PaneKind) {
        let page = {
            let mut pages = self.pages.borrow_mut();
            let Some(index) = pages.iter().position(|(k, _)| *k == kind) else {
                return;
            };
            pages.remove(index).1
        };
        self.syncing.set(true);
        self.view.close_page(&page);
        self.syncing.set(false);
    }

    /// Show `kind`'s page, if there is one.
    fn select(&self, kind: PaneKind) {
        let Some(page) = self.page(kind) else {
            return;
        };
        self.syncing.set(true);
        self.view.set_selected_page(&page);
        self.syncing.set(false);
    }

    fn page(&self, kind: PaneKind) -> Option<adw::TabPage> {
        self.pages
            .borrow()
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, page)| page.clone())
    }

    fn is_empty(&self) -> bool {
        self.pages.borrow().is_empty()
    }
}

pub struct App {
    tabs: Vec<Tab>,
    groups: Vec<Group>,
    active: Option<usize>,
    next_tab_id: usize,
    next_group_id: usize,
    sidebar_visible: bool,
    /// How a group's tabs are ordered in the sidebar and tab bar; persisted.
    sidebar_order: SidebarOrder,
    /// Tab ids in the order the sidebar last rendered them, so an activity
    /// stamp can tell whether the list needs a rebuild or just a relabel.
    shown_order: RefCell<Vec<usize>>,
    /// A running burst of keyboard navigation: the frozen `nav_order` it
    /// started with plus the timeout that ends it. `Rc` so the expiry
    /// timeout can hold a cheap clone (`SourceId` is neither `Clone` nor
    /// `Copy`, so the `RefCell` itself cannot be cloned). See `navigate`.
    nav_burst: Rc<RefCell<NavBurst>>,
    /// Whether a `Msg::Resort` is already scheduled; shared with every tab's
    /// stamp callbacks so a burst of output sends one message, not hundreds.
    resort_pending: Rc<Cell<bool>>,
    style: adw::StyleManager,
    tab_list: gtk::ListBox,
    /// Scroll wrapper around `tab_list`; owns the vertical adjustment used to
    /// keep the active sidebar row on screen.
    list_scroller: gtk::ScrolledWindow,
    tab_bar: gtk::Box,
    /// Scroll wrapper around `tab_bar`; owns the horizontal adjustment used to
    /// keep the active tab on screen.
    tab_scroller: gtk::ScrolledWindow,
    stack: gtk::Stack,
    /// Splits the terminal content box (start) from the active group's pane
    /// area (end). The end child is attached and detached imperatively — only
    /// the active group's pane host is ever parented.
    browser_paned: gtk::Paned,
    /// Wraps the window content; carries the transient failure notices that do
    /// not deserve a dialog.
    toast_overlay: adw::ToastOverlay,
    /// Popover behind the header bar's browser overflow button.
    browser_menu: gtk::Popover,
    /// Popover behind the header bar's primary (hamburger) menu button.
    primary_menu: gtk::Popover,
    /// Endpoint row in the browser overflow menu: dimmed "CDP: unavailable"
    /// until discovery succeeds, then `127.0.0.1:<port>` plus a live copy
    /// button.
    cdp_label: gtk::Label,
    cdp_copy: gtk::Button,
    /// Which group's pane host is currently the paned's end child, if any.
    /// Tracked explicitly so detaching works even after the group was pruned.
    attached_host: Option<usize>,
    /// Groups still waiting for their browser to be restored after a restart,
    /// active group first. Drained one per idle callback.
    pending_browser_restore: Vec<usize>,
    /// The group whose Android is waiting to be restored after a restart.
    /// One per machine, so at most one. Persisted as `android_owner` while
    /// it waits (see `state::android_owner_desired`).
    pending_android_restore: Option<usize>,
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
    /// State of the boot-resume unit, read from the system at startup and
    /// after every change (never persisted). Drives the primary menu's
    /// status line and the dialog behind it.
    autostart: autostart::Status,
    /// Startup check of the local ssh client: remote groups need OpenSSH
    /// >= 8.4, and are disabled (never dropped) otherwise.
    ssh: remote::SshAvailability,
    /// How the workers log in: through an askpass program, or keys only.
    auth: remote::AuthMode,
    /// Remote hosts by their destination string, as the groups name them.
    hosts: HashMap<String, RemoteHost>,
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
    /// Remote sessions whose tab is closed but whose kill the host has not
    /// confirmed yet — an outbox, persisted in the state file and flushed
    /// after the next successful connect to each host.
    pending_kills: Vec<state::PendingKill>,
    /// Set while `rebuild_list` re-selects the active row programmatically.
    /// The sidebar's `row-selected` handler shares it and stays silent while
    /// it is set, so a rebuild never echoes a `Msg::Select` back into the
    /// input queue. Relying on `activate`'s no-op guard instead is not
    /// enough: the echo is queued behind whatever else is pending, and two
    /// queued tab switches then ping-pong forever and freeze the main loop.
    reselecting: Rc<Cell<bool>>,
    /// Icon name for the group drag handle, resolved once against the
    /// display's cached icon theme.
    drag_handle_icon: &'static str,
    /// Per-render host table behind the `tab:<id>:<host-key>` drag payloads:
    /// the key is `local` or an index into this list, so a `:` in a
    /// destination cannot break the payload. Rebuilt by `rebuild_list`.
    host_keys: RefCell<Vec<String>>,
    /// Pages `MODE_TERMINALS` (the tab bar and terminal stack) and
    /// `MODE_OVERVIEW` (the shared overview view); `sync_overview` picks.
    mode_stack: gtk::Stack,
    /// One overview view for every group, re-fed on each switch.
    overview_view: OverviewView,
    /// Which group's data the view currently holds, and that data's
    /// generation; `sync_overview` only re-feeds when either differs.
    fed_overview: Option<(usize, u64)>,
    /// Source of `OverviewData::generation`: app-wide, so a group whose id
    /// is reused after a delete can never match a stale `fed_overview`.
    overview_gen: u64,
    /// Whether a tagger process is running; one at a time.
    tagger_running: bool,
    /// `$XDG_CONFIG_HOME/kabelsalat/overview.json`, read once at startup.
    tagger_config: tagger::Config,
    /// Whether the tagger command's binary was found on PATH at startup; when
    /// not, the overview works without tab links and the panels say so.
    tagging_available: bool,
}

/// Decide whether a sidebar `row-selected` event is a user action that
/// should become `Msg::Select`. `None` while a rebuild is re-selecting
/// programmatically, and for deselection (`row_id == None`).
fn user_selection(reselecting: bool, row_id: Option<usize>) -> Option<usize> {
    if reselecting {
        return None;
    }
    row_id
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
    /// Periodic recomputation of the tab age prefixes.
    AgeTick,
    /// Periodic check for disconnected hosts due for a background retry.
    RetryTick,
    /// Periodic discovery of the claude session running in each tab.
    ClaudeTick,
    /// Coalesced follow-up to an activity stamp: re-sort the sidebar if the
    /// stamp changed the display order. See `request_resort`.
    Resort,
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
    SetSidebarOrder(SidebarOrder),
    ShowHelp,
    SchemeChanged,
    /// The pane-died hook reported a crashed shell: (tab uuid, exit code).
    PaneCrashed(String, i32),
    /// Rerun the shell in a crashed tab (respawn-pane / fresh $SHELL).
    RestartTab(usize),
    /// Open the "tmux unavailable" explanation dialog.
    ShowTmuxWarning,
    /// Re-read the linger state and open the "keep shells running after
    /// logout" dialog for it: the offer while it is off, the fact plus
    /// Disable once it is on.
    ShowLingerWarning,
    /// Run `loginctl enable-linger` (`true`) or `disable-linger` (`false`)
    /// and re-check.
    SetLinger(bool),
    /// Result of the off-thread `set_linger` + re-check: what was asked
    /// for, and the new linger status on success or the failure message.
    LingerChanged(bool, Result<LingerStatus, String>),
    /// Permanently hide the linger warning (persists the dismissal flag).
    DismissLingerWarning,
    /// Open the "resume agent sessions at boot" dialog from the primary menu.
    ShowAutostartDialog,
    /// Install — or repair — the boot-resume unit; enables lingering when
    /// it is off, since a unit without it only runs at login.
    AutostartInstall,
    /// Disable and delete the boot-resume unit. Lingering is left alone.
    AutostartRemove,
    /// Result of the off-thread install or remove: the toast to show, or
    /// the failure message. Either way the state is re-read from the system.
    AutostartChanged(Result<&'static str, String>),
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
    /// Restart restore: bring this group's Android back, hidden.
    RestoreAndroid(usize),
    /// CDP discovery finished for this group's browser: the endpoint, or
    /// `None` when `DevToolsActivePort` never appeared or never parsed.
    CdpReady(usize, Option<browser::CdpEndpoint>),
    /// Copy the active group's CDP endpoint URL to the clipboard.
    CopyCdpEndpoint,
    /// The user picked a tab in a group's pane area: group id, pane. Acted
    /// on only while that group's host is the attached one, so a notify from
    /// a host being disposed (a pruned group) cannot touch another group.
    PaneSelected(usize, PaneKind),
    /// Show the active group's hidden pane area (the header-bar indicator).
    ShowPanes,
    /// Alt+3 / primary menu: open Android in the active group, or bring it to
    /// the front; either way the pane area becomes visible.
    ToggleAndroid,
    /// Stop Android wherever it runs: stop the session, close the pane.
    StopAndroid,
    /// Android finished booting: group id, boot id, adb serial.
    AndroidReady(usize, u64, String),
    /// Android failed to boot or its session exited: group id, boot id,
    /// reason.
    AndroidDied(usize, u64, String),
    /// Paste the clipboard into the focused terminal.
    Paste,
    /// Copy the focused terminal's selection to the clipboard — or, with
    /// nothing selected, open a claude tab (both live on Ctrl+Shift+C).
    Copy,
    /// Capture what the active group's front pane shows.
    Screenshot,
    /// The capture finished: the frame, or why there is none. The id is the
    /// tab that was active when the capture was asked for — the readback can
    /// take seconds, and the paste belongs to that tab or to none.
    ///
    /// The error is already rendered to text, like [`Msg::LingerChanged`]'s:
    /// klamottenkiste's `CaptureError` is not `Clone`, and both failure kinds
    /// mean the same thing here — a toast and a line on stderr.
    ScreenshotCaptured(usize, Result<CapturedFrame, String>),
    /// A `kabelsalat run` invocation: create a tab in the group named by
    /// `group` — an existing one, or a new one created here for
    /// `--create` — running `argv` in `cwd`. Deliberately inert otherwise: it
    /// does not activate the tab, raise the window, change the active group,
    /// or touch the group's browser pane, because the user may be typing
    /// somewhere else when an agent fires this.
    SpawnCommand {
        group: crate::cli::GroupTarget,
        tab_uuid: String,
        cwd: Option<PathBuf>,
        argv: Vec<String>,
    },
    /// A `kabelsalat rename` invocation: set the group's name. Like
    /// `SpawnCommand` it changes no focus and raises no window.
    RenameGroup {
        group_uuid: String,
        name: String,
    },
    /// A `kabelsalat browser` invocation: bring up the group's browser if it
    /// has none. Like a restored browser it comes up hidden unless the
    /// group is the active one, and like `SpawnCommand` it takes no focus,
    /// raises no window and changes no active group.
    OpenBrowser {
        group_uuid: String,
    },
    /// A `kabelsalat android` invocation: bring up Android in the group if
    /// nobody has it. Like `OpenBrowser` it takes no focus, raises no window
    /// and changes no active group; it becomes the front pane only as the
    /// group's first pane, and the area only appears for the active group.
    OpenAndroid {
        group_uuid: String,
    },
    /// A `kabelsalat overview root` invocation: set or clear the group's
    /// overview root. It changes no mode, no active group and no focus.
    SetOverviewRoot {
        group_uuid: String,
        root: Option<PathBuf>,
    },
    /// The header toggle: show the active group's overview (`true`) or its
    /// terminals. A no-op when the active group is remote or already in that
    /// mode — `#[watch] set_active` echoes every switch back through here.
    SetOverviewMode(bool),
    /// A tab chip or tabs-panel row was clicked in the overview: switch that
    /// tab's group to Terminals and make the tab (by uuid) active.
    ShowTab(String),
    /// The Resolve button of the active group's data issue at this index
    /// (into the list the view was last given). Spawns a `claude` tab the way
    /// `kabelsalat run` does: no focus change, no group switch.
    ResolveIssue(usize),
    /// A link in a rendered node body that names no node: open it with the
    /// default handler.
    OpenOverviewUri(String),
    /// A directory monitor saw a `*.md` file below a group's overview root
    /// change (`removed: false`) or go away (`removed: true`).
    OverviewFileChanged {
        group_id: usize,
        path: PathBuf,
        removed: bool,
    },
    /// A directory appeared below a group's overview root: watch it and
    /// take its files into the graph.
    OverviewDirCreated {
        group_id: usize,
        path: PathBuf,
    },
    /// Periodic scan for tabs whose transcript grew enough to re-tag.
    OverviewScan,
    /// The off-thread tagger finished for a tab: its uuid, the watermark the
    /// run covered (rendered), and stdout or the failure. A failure keeps the
    /// tab's previous tags and mark.
    TaggerDone {
        tab_uuid: String,
        mark: String,
        result: Result<String, String>,
    },
    /// A remote host's worker reported back.
    Remote {
        host: String,
        event: RemoteEvent,
    },
    /// Rerun the connect sequence for a host — or, when it is live, reattach
    /// its detached tabs.
    Reconnect(String),
    /// Ctrl+Shift+H / the sidebar button: open the create form for a remote
    /// group, or explain why remote groups are unavailable.
    NewRemoteGroupDialog,
    /// Apply the create form: a new group on `host`, its first tab, and the
    /// host's connect. `default_url` is already normalized.
    CreateRemoteGroup {
        host: String,
        name: String,
        default_url: Option<String>,
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

                // "Terminals | Overview": the active group's mode. Two linked
                // toggle buttons rather than adw::ToggleGroup, which needs
                // libadwaita 1.7. `#[watch] set_active` echoes a toggled
                // signal back as Msg::SetOverviewMode, which is therefore a
                // no-op when the mode is unchanged. Remote groups have no
                // overview: both segments go insensitive with a tooltip.
                #[wrap(Some)]
                set_title_widget = &gtk::Box {
                    add_css_class: "linked",
                    set_valign: gtk::Align::Center,

                    append: terminals_button = &gtk::ToggleButton {
                        set_label: "Terminals",
                        #[watch]
                        set_active: !model.active_overview_mode(),
                        #[watch]
                        set_sensitive: !model.active_group_is_remote(),
                        #[watch]
                        set_tooltip_text: model
                            .active_group_is_remote()
                            .then_some("Remote groups have no overview"),
                        connect_toggled[sender] => move |button| {
                            if button.is_active() {
                                sender.input(Msg::SetOverviewMode(false));
                            }
                        },
                    },
                    append = &gtk::ToggleButton {
                        set_label: "Overview",
                        set_group: Some(&terminals_button),
                        #[watch]
                        set_active: model.active_overview_mode(),
                        #[watch]
                        set_sensitive: !model.active_group_is_remote(),
                        #[watch]
                        set_tooltip_text: model
                            .active_group_is_remote()
                            .then_some("Remote groups have no overview"),
                        connect_toggled[sender] => move |button| {
                            if button.is_active() {
                                sender.input(Msg::SetOverviewMode(true));
                            }
                        },
                    },
                },

                // The first pack_end sits rightmost: the primary menu, where
                // app-wide choices live. Built like the browser overflow
                // menu — a popover of flat buttons — since three entries do
                // not justify a gio::Menu and the action plumbing it needs.
                pack_end = &gtk::MenuButton {
                    set_icon_name: "open-menu-symbolic",
                    set_tooltip_text: Some("Main menu"),

                    #[wrap(Some)]
                    set_popover = &primary_menu.clone() {
                        #[wrap(Some)]
                        set_child = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 2,

                            gtk::Button {
                                add_css_class: "flat",
                                #[watch]
                                set_sensitive: model.autostart != autostart::Status::Unavailable,
                                #[watch]
                                set_tooltip_text: (model.autostart == autostart::Status::Unavailable)
                                    .then_some("Needs tmux and a systemd user manager"),
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::ShowAutostartDialog);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::Label {
                                        set_label: "Resume agent sessions at boot…",
                                        set_halign: gtk::Align::Start,
                                    },
                                    gtk::Label {
                                        #[watch]
                                        set_label: autostart::status_label(model.autostart),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "dim-label",
                                        add_css_class: "caption",
                                    },
                                },
                            },

                            // The warning icon is the first-time hint; this
                            // is the way back once it has been dismissed.
                            gtk::Button {
                                add_css_class: "flat",
                                #[watch]
                                set_sensitive: model.tmux.is_some()
                                    && model.linger != LingerStatus::NotApplicable,
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::ShowLingerWarning);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    gtk::Label {
                                        set_label: "Keep shells running after logout…",
                                        set_halign: gtk::Align::Start,
                                    },
                                    gtk::Label {
                                        #[watch]
                                        set_label: autostart::linger_status_label(model.linger),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "dim-label",
                                        add_css_class: "caption",
                                    },
                                },
                            },

                            gtk::Button {
                                add_css_class: "flat",
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::ToggleAndroid);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Label {
                                    set_label: "Open Android pane (Alt+3)",
                                    set_halign: gtk::Align::Start,
                                },
                            },

                            gtk::Button {
                                add_css_class: "flat",
                                #[watch]
                                set_sensitive: model.android_running(),
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::StopAndroid);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Label {
                                    set_label: "Stop Android",
                                    set_halign: gtk::Align::Start,
                                },
                            },

                            gtk::Button {
                                add_css_class: "flat",
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::ShowHelp);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Label {
                                    set_label: "Keyboard shortcuts (F1)",
                                    set_halign: gtk::Align::Start,
                                },
                            },
                        },
                    },
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
                    set_tooltip_text: Some("Panes hidden — click to show them"),
                    #[watch]
                    set_visible: model.active_panes_hidden(),
                    connect_clicked => Msg::ShowPanes,
                },

                pack_end = &gtk::Button {
                    set_icon_name: "camera-photo-symbolic",
                    set_tooltip_text: Some("Screenshot the front pane → paste into terminal"),
                    // The point of the button is to leave the user typing a
                    // description next to the pasted image, so it must not
                    // keep the keyboard: with focus on the button, the next
                    // Space or Enter would fire a second capture and a second
                    // Ctrl-V instead of reaching the terminal.
                    set_focus_on_click: false,
                    #[watch]
                    set_sensitive: model.active_front_running(),
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

                                gtk::ToggleButton {
                                    set_icon_name: "view-sort-descending-symbolic",
                                    set_tooltip_text: Some("Sort tabs by activity (off: manual order, newest on top)"),
                                    #[watch]
                                    set_active: model.sidebar_order == SidebarOrder::Activity,
                                    connect_toggled[sender] => move |button| {
                                        let order = if button.is_active() {
                                            SidebarOrder::Activity
                                        } else {
                                            SidebarOrder::Manual
                                        };
                                        sender.input(Msg::SetSidebarOrder(order));
                                    },
                                },

                                gtk::Button {
                                    set_icon_name: "folder-new-symbolic",
                                    set_tooltip_text: Some("New group (Ctrl+Shift+N)"),
                                    connect_clicked => Msg::NewGroup,
                                },

                                gtk::Button {
                                    set_icon_name: "network-server-symbolic",
                                    #[watch]
                                    set_sensitive: model.ssh.is_available(),
                                    #[watch]
                                    set_tooltip_text: Some(model.remote_button_tooltip().as_str()),
                                    connect_clicked => Msg::NewRemoteGroupDialog,
                                },

                                gtk::Button {
                                    set_icon_name: "tab-new-symbolic",
                                    set_tooltip_text: Some("New tab (Ctrl+Shift+T)"),
                                    connect_clicked => Msg::NewTab,
                                },
                            },

                            append = &list_scroller.clone() {
                                set_vexpand: true,
                                set_hscrollbar_policy: gtk::PolicyType::Never,

                                #[local_ref]
                                #[wrap(Some)]
                                set_child = &tab_list -> gtk::ListBox {
                                    add_css_class: "navigation-sidebar",
                                    connect_row_selected[sender, reselecting] => move |_, row| {
                                        let row_id = row.and_then(|r| r.widget_name().parse().ok());
                                        if let Some(id) = user_selection(reselecting.get(), row_id) {
                                            sender.input(Msg::Select(id));
                                        }
                                    },
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

                    // The mode stack: the terminal area, or the active group's
                    // overview (`sync_overview` picks the page). The terminal
                    // box is never re-parented — unparenting VTE disturbs its
                    // focus and size — so the overview is a sibling page.
                    #[wrap(Some)]
                    set_start_child = &mode_stack.clone() {
                        set_hexpand: true,
                        set_vexpand: true,

                        add_named[Some(MODE_TERMINALS)] = &gtk::Box {
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
                    // A server started at boot (`kabelsalat resume`) carries
                    // the user manager's minimal environment; give it the
                    // desktop's, so sessions created from here inherit it.
                    if let Err(err) = ctl.sync_global_environment(tmuxctl::process_environment()) {
                        eprintln!("kabelsalat: syncing the tmux server environment: {err}");
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
        // Read from the system, never remembered: the unit file, whether it
        // is enabled, and lingering are the truth about what happens at boot.
        let boot_resume = autostart::detect(tmux.is_some() && autostart::has_systemctl(), linger);

        // The symbolic handle may be absent in a sparse icon theme; fall back
        // to a menu glyph so the affordance never renders as a broken image.
        // Resolved once: `IconTheme::for_display` is the cached theme, while
        // `IconTheme::default()` is `gtk_icon_theme_new()` and re-reads every
        // index.theme on disk per call.
        let drag_handle_icon = match gtk::gdk::Display::default() {
            Some(display)
                if gtk::IconTheme::for_display(&display).has_icon("list-drag-handle-symbolic") =>
            {
                "list-drag-handle-symbolic"
            }
            _ => "open-menu-symbolic",
        };

        // Checked once, like tmux. Remote groups need OpenSSH >= 8.4 for
        // SSH_ASKPASS_REQUIRE; anything else disables them.
        let ssh = remote_worker::detect_ssh();
        let auth = remote::auth_mode(|key| std::env::var(key).ok(), remote_worker::is_executable);

        // The tagger configuration, read once like everything else here; a
        // command whose binary is not on PATH leaves the overview without
        // tab links rather than failing a run every minute.
        let tagger_config = autostart::config_home()
            .map(|home| tagger::load_config(&tagger::config_path(&home)))
            .unwrap_or_default();
        let tagging_available = tagger_config
            .tagger_command
            .first()
            .is_some_and(|program| on_path(program, std::env::var_os("PATH").as_deref()));
        // The overview's callbacks only queue messages: the view never
        // touches the model, and every switch goes through `update`.
        let overview_view = OverviewView::new(Callbacks {
            open_tab: Rc::new({
                let input = sender.input_sender().clone();
                move |uuid: String| {
                    let _ = input.send(Msg::ShowTab(uuid));
                }
            }),
            resolve: Rc::new({
                let input = sender.input_sender().clone();
                move |index: usize| {
                    let _ = input.send(Msg::ResolveIssue(index));
                }
            }),
            open_uri: Rc::new({
                let input = sender.input_sender().clone();
                move |uri: String| {
                    let _ = input.send(Msg::OpenOverviewUri(uri));
                }
            }),
        });

        let mut model = App {
            tabs: Vec::new(),
            groups: Vec::new(),
            active: None,
            next_tab_id: 1,
            next_group_id: 1,
            sidebar_visible: true,
            sidebar_order: SidebarOrder::default(),
            shown_order: RefCell::new(Vec::new()),
            nav_burst: Rc::new(RefCell::new(None)),
            resort_pending: Rc::new(Cell::new(false)),
            style,
            tab_list: gtk::ListBox::new(),
            list_scroller: gtk::ScrolledWindow::new(),
            tab_bar: gtk::Box::new(gtk::Orientation::Horizontal, 2),
            tab_scroller: gtk::ScrolledWindow::new(),
            stack: gtk::Stack::new(),
            browser_paned: gtk::Paned::new(gtk::Orientation::Horizontal),
            toast_overlay: adw::ToastOverlay::new(),
            browser_menu: gtk::Popover::new(),
            primary_menu: gtk::Popover::new(),
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
            attached_host: None,
            pending_browser_restore: Vec::new(),
            pending_android_restore: None,
            browser_poll_running: false,
            browser_restore_error_shown: false,
            window: root.clone(),
            input: sender.input_sender().clone(),
            tmux,
            availability,
            linger,
            linger_dismissed,
            autostart: boot_resume,
            ssh,
            auth,
            hosts: HashMap::new(),
            select_hint_visible: false,
            select_hint_gen: 0,
            monitor: None,
            state_path: state::state_file(),
            state_dir: state::state_dir(),
            // Set for real in `restore_or_fresh`, which is the load that the
            // model is actually built from.
            uuids_unpersisted: Cell::new(false),
            save_error_shown: Cell::new(false),
            pending_kills: Vec::new(),
            reselecting: Rc::new(Cell::new(false)),
            drag_handle_icon,
            host_keys: RefCell::new(Vec::new()),
            mode_stack: gtk::Stack::new(),
            overview_view,
            fed_overview: None,
            overview_gen: 0,
            tagger_running: false,
            tagger_config,
            tagging_available,
        };

        let tab_list = model.tab_list.clone();
        let mode_stack = model.mode_stack.clone();
        let reselecting = model.reselecting.clone();
        let list_scroller = model.list_scroller.clone();
        let tab_bar = model.tab_bar.clone();
        let tab_scroller = model.tab_scroller.clone();
        let stack = model.stack.clone();
        let browser_paned = model.browser_paned.clone();
        let toast_overlay = model.toast_overlay.clone();
        let browser_menu = model.browser_menu.clone();
        let primary_menu = model.primary_menu.clone();
        // An enabled unit that no longer matches this build (the binary
        // moved, a newer template) is rewritten silently, as tmux.conf is;
        // the dialog's Repair only remains for a rewrite that failed.
        if model.autostart == autostart::Status::Stale {
            model.autostart_change(autostart::install, "Boot-resume unit repaired");
        }
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
        // The overview page sits next to the terminal box built in `view!`;
        // the stack starts on the terminals and `sync_overview` switches it.
        model
            .mode_stack
            .add_named(&model.overview_view.widget(), Some(MODE_OVERVIEW));
        model.mode_stack.set_visible_child_name(MODE_TERMINALS);

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
        if let Some(events_dir) = model.tmux.as_ref().and_then(|ctl| ctl.events_dir()) {
            let events_dir = events_dir.to_path_buf();
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
        // Always on, unlike the browser poll: every tab has an age.
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_seconds_local(AGE_TICK_SECS, move || {
            match input.send(Msg::AgeTick) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                // The component is gone; stop ticking.
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        // Always on too: a host can drop at any time. A tick with nothing
        // disconnected only walks the host map.
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_seconds_local(remote::RETRY_TICK_SECS, move || {
            match input.send(Msg::RetryTick) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        // Always on: a remote host's panes are inspected by its worker even
        // when there is no local tmux (locally the tick is then a no-op).
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_seconds_local(CLAUDE_TICK_SECS, move || {
            match input.send(Msg::ClaudeTick) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        // The tagger's periodic trigger (the other is a title change). A tick
        // with nothing due only stats a transcript per tab.
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_seconds_local(tagger::SCAN_SECS, move || {
            match input.send(Msg::OverviewScan) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
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
            // Picking a tab in the sidebar or tab bar shows that tab: its
            // group leaves overview mode (spec: activating a tab switches the
            // group to Terminals). Keyboard navigation and the group jump go
            // through `activate` directly and keep each group's own mode.
            Msg::Select(id) => {
                let left_overview = self.leave_overview_for(id);
                if !self.activate(id) {
                    if !left_overview {
                        return; // already active: nothing changed, nothing to save
                    }
                    // Already the active tab, but its group just switched to
                    // Terminals: activate() skipped the funnel, so sync here.
                    self.sync_overview();
                }
            }
            Msg::CloseTab(id) => self.close_tab(id),
            Msg::CloseActive => {
                if let Some(id) = self.active {
                    self.close_tab(id);
                }
            }
            Msg::ChildExited(id, status) => {
                if self.tab_host(id).is_some() {
                    // Remote: the worker asks the host, never this thread.
                    self.remote_child_exited(id, status);
                } else if let Some(tmux) = &self.tmux {
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
                            let pairs = self.group_env_pairs(group).unwrap_or_default();
                            let env: Vec<(&str, &str)> =
                                pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                            // Likewise for a claude the tab had: ignored by
                            // a genuine reattach, resumed if -A creates.
                            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                                let resume = tab.claude.as_ref().map(|c| c.resume_argv());
                                let cwd = tab.claude.as_ref().map(|c| c.cwd.as_path());
                                tab.armed.respawn();
                                spawn_backing(
                                    &tab.terminal,
                                    &uuid,
                                    Some(tmux),
                                    cwd,
                                    resume.as_deref(),
                                    &env,
                                );
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
            Msg::SetLinger(enabled) => self.set_linger(enabled),
            Msg::LingerChanged(wanted, result) => self.on_linger_changed(wanted, result),
            Msg::DismissLingerWarning => self.linger_dismissed = true,
            // The boot-resume state lives on the system, not in the state
            // file, so none of these four need the save at the bottom.
            Msg::ShowAutostartDialog => {
                self.show_autostart_dialog();
                return;
            }
            Msg::AutostartInstall => {
                self.autostart_change(autostart::install, "Boot resume installed");
                // A unit without lingering only runs at login; installing
                // from the dialog means "at boot", so the two go together.
                if self.linger == LingerStatus::Disabled {
                    self.set_linger(true);
                }
                return;
            }
            Msg::AutostartRemove => {
                self.autostart_change(autostart::remove, "Boot resume removed");
                return;
            }
            Msg::AutostartChanged(result) => {
                self.refresh_autostart();
                match result {
                    Ok(done) => self.show_toast(done),
                    Err(err) => self.show_notice(&err),
                }
                return;
            }
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
            Msg::RestoreAndroid(group) => self.restore_android(group),
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
                // The CLI's snapshot carries the endpoint, so `kabelsalat
                // browser` can answer with it from the moment it exists.
                self.publish_groups();
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
            // Runtime only, like the CDP messages: the front pane is not
            // persisted, so this skips the save at the bottom.
            Msg::PaneSelected(group_id, kind) => {
                if self.attached_host != Some(group_id) {
                    return;
                }
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) {
                    group.front_pane = kind;
                }
                self.focus_panes_or_terminal();
                return;
            }
            Msg::ShowPanes => {
                if let Some(id) = self.active_group()
                    && let Some(group) = self.groups.iter_mut().find(|g| g.id == id)
                    && group.has_panes()
                {
                    group.panes_visible = true;
                }
                self.sync_pane_host();
                self.focus_panes_or_terminal();
            }
            Msg::ToggleAndroid => self.toggle_android(),
            Msg::StopAndroid => self.stop_android(),
            // Runtime state only (the owner was persisted at spawn), like
            // CdpReady.
            Msg::AndroidReady(group_id, boot, serial) => {
                self.android_ready(group_id, boot, serial);
                return;
            }
            Msg::AndroidDied(group_id, boot, reason) => self.android_died(group_id, boot, &reason),
            // The shortcut controller is global-scope, so it fires wherever
            // focus sits. Only the terminal that actually has the keyboard may
            // act on it — otherwise a Ctrl-Shift-V aimed at the browser pane
            // would type into whichever tab happens to be active.
            Msg::Paste => {
                if let Some(tab) = self.active_tab().filter(|t| t.terminal.has_focus()) {
                    tab.terminal.paste_clipboard();
                }
                return;
            }
            // With a selection in the focused terminal this is Copy, which
            // touches only the clipboard; otherwise it opens a claude tab,
            // a layout change that falls through to the save.
            Msg::Copy => {
                if let Some(tab) = self
                    .active_tab()
                    .filter(|t| t.terminal.has_focus() && t.terminal.has_selection())
                {
                    tab.terminal.copy_clipboard_format(vte4::Format::Text);
                    return;
                }
                let group = self
                    .active_tab()
                    .map(|t| t.group)
                    .unwrap_or_else(|| self.groups[0].id);
                self.open_claude_tab(group, &sender);
            }
            // The clipboard and the pty are the only things this touches, and
            // neither is persisted — so both arms return early like the other
            // runtime-only messages.
            Msg::Screenshot => {
                self.capture_front_pane(&sender);
                return;
            }
            Msg::ScreenshotCaptured(tab, result) => {
                match result {
                    Ok(frame) => self.paste_screenshot(tab, frame),
                    Err(detail) => {
                        eprintln!("kabelsalat: pane screenshot failed: {detail}");
                        self.show_toast("The screenshot failed.");
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
            // The age stamps ride along with whatever writes next — a tab
            // create/close/move, or shutdown. (The v2 spec guessed that a
            // title change already saves; it does not, see the arm above.
            // Piggybacking on the layout mutations is the deliberate
            // alternative: an unclean exit then loses the stamps since the
            // last structural change, and the tabs seed slightly young — the
            // direction the spec sanctions erring in.) The tick itself must
            // never reach the save at the bottom of `update`: a disk write
            // every 30 s would be a regression.
            Msg::AgeTick => {
                self.refresh_ages();
                self.resort_if_stale();
                // The chips on the map carry the same ages.
                self.refresh_overview_tabs();
                return;
            }
            // Like the age tick: starting a retry writes nothing; its result
            // arrives as Msg::Remote, which persists as usual.
            Msg::RetryTick => {
                self.retry_hosts();
                return;
            }
            // Saves only when a tab's claude session changed, never per tick.
            Msg::ClaudeTick => {
                self.refresh_claude_sessions();
                return;
            }
            // Like the tick: a re-sort is a paint, never a disk write.
            Msg::Resort => {
                self.resort_pending.set(false);
                // The stamp that got us here invalidated the stamped tab's
                // cached age prefix; relabel before the rebuild paints it.
                self.refresh_ages();
                self.resort_if_stale();
                return;
            }
            Msg::MoveTabPicker => self.show_group_picker(PickerMode::Move),
            Msg::MoveTabTo(target) => self.move_active_tab(target),
            Msg::JumpPicker => self.show_group_picker(PickerMode::Jump),
            Msg::JumpToGroup(id) => {
                if let Some(first) = self.group_members(id).first().map(|t| t.id) {
                    self.activate(first);
                }
            }
            Msg::DropTab { src, dest } => self.drop_tab(src, dest),
            Msg::DropGroup { src, dest } => self.drop_group(src, dest),
            Msg::DropTabOnGroup { src, group } => self.drop_tab_on_group(src, group),
            Msg::GroupSettingsDialog => self.show_group_settings(SettingsMode::Edit),
            Msg::NewRemoteGroupDialog => {
                match self.ssh.reason() {
                    Some(reason) => self.show_notice(&reason),
                    None => self.show_group_settings(SettingsMode::Create),
                }
                return;
            }
            Msg::CreateRemoteGroup {
                host,
                name,
                default_url,
            } => {
                // The form only enables Create for a valid host; this guards
                // the message itself.
                if state::validate_host(&host).is_err() {
                    return;
                }
                let id = self.create_group();
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    let name = name.trim();
                    // An empty name defaults to the host.
                    group.name = if name.is_empty() {
                        host.clone()
                    } else {
                        name.to_string()
                    };
                    group.host = Some(host.clone());
                    group.default_url = default_url;
                }
                // The tab appears at once, behind "Connecting…"; it spawns when
                // the host is live, or shows why it is not.
                self.open_tab(id, &sender);
                self.connect_host(&host);
            }
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
            Msg::SetSidebarOrder(order) => self.set_sidebar_order(order),
            Msg::SetSidebar(visible) => self.set_sidebar(visible),
            Msg::ShowHelp => self.show_help(),
            Msg::SchemeChanged => {
                for tab in &self.tabs {
                    apply_scheme(&tab.terminal, self.style.is_dark());
                }
            }
            Msg::OpenBrowser { group_uuid } => self.open_browser(&group_uuid),
            Msg::OpenAndroid { group_uuid } => self.open_android(&group_uuid),
            Msg::SpawnCommand {
                group,
                tab_uuid,
                cwd,
                argv,
            } => {
                // `--create` resolved the cwd against the caller's local
                // directory; see `cli_spawn_cwd`.
                let via_create = matches!(group, crate::cli::GroupTarget::Create { .. });
                let group_id = match group {
                    // The snapshot the CLI validated against is a copy, so the
                    // group can in principle be gone by the time this arrives.
                    crate::cli::GroupTarget::Existing(group_uuid) => {
                        let Some(id) = self
                            .groups
                            .iter()
                            .find(|g| g.uuid == group_uuid)
                            .map(|g| g.id)
                        else {
                            eprintln!("spawn request for unknown group {group_uuid}");
                            return;
                        };
                        id
                    }
                    // `--create`: a new group at the end of the list, named
                    // and otherwise left alone — no activation, no raise.
                    crate::cli::GroupTarget::Create { name } => {
                        // The CLI decided "nothing matches" against a snapshot
                        // copy. Two invocations racing that copy would both
                        // land here and make two groups with the same name,
                        // which no later `-g <name>` could tell apart. Recheck
                        // the live model: a unique match is spawned into, as
                        // the spec's "unique match" row says.
                        let existing: Vec<usize> = self
                            .groups
                            .iter()
                            .filter(|g| g.name == name)
                            .map(|g| g.id)
                            .collect();
                        match existing.as_slice() {
                            [only] => *only,
                            _ => {
                                let id = self.create_group();
                                if let Some(created) = self.groups.iter_mut().find(|g| g.id == id) {
                                    created.name = name;
                                }
                                id
                            }
                        }
                    }
                };
                let title = crate::cli::command_title(&argv);
                let cwd = cli_spawn_cwd(
                    self.group_host(group_id).is_some(),
                    via_create,
                    cwd,
                    Path::is_dir,
                );
                let id = self.add_tab(
                    tab_uuid,
                    group_id,
                    Some(title),
                    None,
                    cwd.as_deref(),
                    Some(&argv),
                    &sender,
                );
                self.move_tab_to_group_front(id);
                // add_tab alone leaves the sidebar stale; the usual funnel for
                // that is activate(), which we deliberately do not call. The
                // bottom-of-update save_state() below still runs, since this
                // arm does not return early.
                self.rebuild_list();
            }
            Msg::SetOverviewRoot { group_uuid, root } => {
                // Same race as SpawnCommand: the CLI validated against a copy.
                let Some(group) = self.groups.iter_mut().find(|g| g.uuid == group_uuid) else {
                    eprintln!("overview root request for unknown group {group_uuid}");
                    return;
                };
                group.overview_root = root;
                let group_id = group.id;
                // Reload now (or drop the data and its monitors on --clear),
                // and redraw if the group's overview is on screen. Never a
                // mode change: the user flips the header toggle themselves.
                self.load_overview(group_id);
                if self.active_group() == Some(group_id) {
                    self.sync_overview();
                }
                // save_state() at the bottom of update() persists the root
                // and republishes the snapshot (issues included) the CLI reads.
            }
            Msg::SetOverviewMode(on) => {
                let Some(group_id) = self.active_group() else {
                    return;
                };
                if self.group_host(group_id).is_some() {
                    return; // remote groups have no overview
                }
                let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) else {
                    return;
                };
                if group.overview_mode == on {
                    return; // the #[watch] echo of a switch: nothing changed
                }
                group.overview_mode = on;
                self.sync_overview();
                // The mode is persisted per group: fall through to the save.
            }
            Msg::ShowTab(uuid) => {
                let Some(id) = self.tabs.iter().find(|t| t.uuid == uuid).map(|t| t.id) else {
                    return; // the tab closed since the chip was drawn
                };
                self.leave_overview_for(id);
                if !self.activate(id) {
                    // Already active: activate() skipped the funnel.
                    self.sync_overview();
                }
            }
            Msg::ResolveIssue(index) => {
                // Hands the prepared prompt to the SpawnCommand arm, the
                // exact `kabelsalat run` path: no activate, no raise, no
                // active-group change. That arm saves; this one changed nothing.
                if let Some(msg) = self.resolve_issue_command(index) {
                    sender.input(msg);
                }
                return;
            }
            Msg::OpenOverviewUri(uri) => {
                gtk::UriLauncher::new(&uri).launch(
                    Some(&self.window),
                    gio::Cancellable::NONE,
                    move |result| {
                        if let Err(err) = result {
                            eprintln!("failed to open {uri}: {err}");
                        }
                    },
                );
                return;
            }
            // Node files are the repository's, not the state file's: a
            // change redraws and republishes the issues, never saves.
            Msg::OverviewFileChanged {
                group_id,
                path,
                removed,
            } => {
                self.overview_file_changed(group_id, &path, removed);
                return;
            }
            Msg::OverviewDirCreated { group_id, path } => {
                self.overview_dir_created(group_id, &path);
                return;
            }
            // Like the other ticks: starting a run writes nothing.
            Msg::OverviewScan => {
                self.overview_scan();
                return;
            }
            // Tags live on the tmux session (or in memory), never in the
            // state file: no save here either.
            Msg::TaggerDone {
                tab_uuid,
                mark,
                result,
            } => {
                self.tagger_done(&tab_uuid, &mark, result);
                return;
            }
            Msg::RenameGroup { group_uuid, name } => {
                // Same race as SpawnCommand: the CLI validated against a copy.
                let Some(group) = self.groups.iter_mut().find(|g| g.uuid == group_uuid) else {
                    eprintln!("rename request for unknown group {group_uuid}");
                    return;
                };
                let group_id = group.id;
                // The CLI's duplicate refusal was decided against a snapshot
                // copy, so two renames racing it could both pick the same
                // name. Recheck against the live model and drop the loser,
                // rather than leaving two groups the CLI cannot tell apart.
                if let Some(clash) = self
                    .groups
                    .iter()
                    .find(|g| g.id != group_id && g.name == name)
                {
                    eprintln!(
                        "rename refused: a group named '{name}' already exists ({})",
                        clash.uuid
                    );
                    return;
                }
                let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) else {
                    return;
                };
                group.name = name;
                // save_state() at the bottom of update() republishes the
                // snapshot the CLI reads; rebuild_list() redraws the header.
                self.rebuild_list();
            }
            // A discovery answer saves only when it changed a tab, like the
            // local tick; every other host event is a layout change.
            Msg::Remote {
                host,
                event: RemoteEvent::ClaudeSessions(found),
            } => {
                if !self.on_claude_sessions(&host, found) {
                    return;
                }
                self.rebuild_list();
            }
            Msg::Remote { host, event } => self.on_remote_event(host, event),
            Msg::Reconnect(host) => self.connect_host(&host),
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
        self.attached_host = None;
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
        // Remote sessions outlive the app like local ones; only the ssh
        // masters go. The next start logs in again.
        for host in self.hosts.values() {
            if let Some(link) = &host.link {
                link.worker.exit_master();
            }
        }
        // One shared SIGTERM deadline for all of them: serial teardown cost
        // `n * TERM_GRACE` of frozen UI, this costs at most TERM_GRACE however
        // many browsers there are. `Keep` preserves every profile for restore.
        browser::shutdown_all(
            self.groups.iter_mut().filter_map(|g| g.browser.as_mut()),
            browser::TERM_GRACE,
            ProfileDisposition::Keep,
        );
        // Android's display dies with this process, so its session is
        // stopped too. The `Android` stays on its group, torn down, so the
        // save below still records the owner and the next start restores it.
        let android_groups: Vec<usize> = self
            .groups
            .iter()
            .filter(|g| g.android.is_some())
            .map(|g| g.id)
            .collect();
        for id in android_groups {
            self.android_env_unset(id);
        }
        for group in &mut self.groups {
            if let Some(android) = &mut group.android {
                android.teardown();
            }
        }
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

    /// The host a group's tabs run on; `None` for a local group (or an
    /// unknown id). Owned, so callers can hold it across `&mut self` calls.
    fn group_host(&self, group: usize) -> Option<String> {
        self.groups
            .iter()
            .find(|g| g.id == group)
            .and_then(|g| g.host.clone())
    }

    /// Tab ids in display order: groups in sidebar order, each group's tabs
    /// as `group_members` shows them. Keyboard navigation and the
    /// close-neighbour pick follow this so they match the sidebar.
    fn nav_order(&self) -> Vec<usize> {
        self.groups
            .iter()
            .flat_map(|g| self.group_members(g.id).into_iter().map(|t| t.id))
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
            android: None,
            panes_visible: false,
            front_pane: PaneKind::Browser,
            panes: PaneHost::new(&self.input, id),
            browser_split: state::DEFAULT_BROWSER_SPLIT,
            default_url: None,
            host: None,
            overview_root: None,
            overview_mode: false,
            overview: None,
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
        self.sidebar_order = saved.sidebar_order;
        // Kept whether or not any tab comes back: a queued kill belongs to a
        // host, not to a tab, and must survive until that host confirms it.
        self.pending_kills = saved.pending_kills.clone();

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

        // The local bucket only: remote tabs are planned per host once that
        // host answers (on_host_connected), and never respawned here.
        let plan = state::reconcile_local(&saved, &live, &dead);
        if saved.tabs.is_empty() && plan.adopt.is_empty() {
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
                android: None,
                // Restored as "wanted"; the actual pane comes up later, one
                // group at a time, in start_browser_maintenance().
                panes_visible: group.browser_open,
                front_pane: PaneKind::Browser,
                panes: PaneHost::new(&self.input, group.id),
                browser_split: group.browser_split,
                default_url: group.default_url.clone(),
                host: group.host.clone(),
                overview_root: group.overview_root.clone(),
                overview_mode: group.overview_mode,
                overview: None,
            });
        }
        self.next_group_id = saved.groups.iter().map(|g| g.id).max().map_or(1, |m| m + 1);

        // Saved tabs, in order; attach live ones (crashed if the pane died),
        // respawn the rest. `spawn_argv -A` attaches or creates uniformly.
        // A local tab whose session is gone and that had a claude in it is
        // recreated as that claude, resumed in its directory, rather than
        // as a shell: `-A` ignores both for a session that survived, so the
        // check against `live` only spares a surviving tab the arguments.
        let dead_exit = |uuid: &str| dead.iter().find(|d| d.uuid == uuid).map(|d| d.exit_code);
        // A remote tab's decision waits for its host's listing
        // (on_host_connected); `live` is the local server's.
        for tab in &saved.tabs {
            let resume = match state::group_host(&saved, tab.group) {
                None => state::resume_on_restore(tab.claude.as_ref(), &tab.uuid, &live),
                Some(_) => None,
            };
            let command = resume.map(|c| c.resume_argv());
            let id = self.add_tab(
                tab.uuid.clone(),
                tab.group,
                // `last_title` is the dedupe anchor, and `Tab::title` is what
                // `update_title` compares against — so restoring the anchor is
                // exactly restoring the title. Both were written from the same
                // `tab.title`, so they only differ for state files predating
                // the field, where `title` is the honest fallback.
                Some(tab.last_title.clone().unwrap_or_else(|| tab.title.clone())),
                dead_exit(&tab.uuid),
                resume.map(|c| c.cwd.as_path()),
                command.as_deref(),
                sender,
            );
            // Carried over as is: discovery confirms or clears it once the
            // tab's processes are visible again.
            if let Some(restored) = self.tabs.iter_mut().find(|t| t.id == id) {
                restored.claude = tab.claude.clone();
            }
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
        // Tags survive a GUI restart on the tmux server: read them back from
        // every reattached session. Anything unreadable simply starts untagged.
        self.restore_tags(&reattached);

        // Seed the ages from the state file — the only source that survives a
        // restart. A tab whose entry predates the field (or an adopted orphan,
        // which has no `SavedTab` at all) keeps the `now` set at construction:
        // erring young once is better than inventing a past.
        let seeds: HashMap<&str, u64> = saved
            .tabs
            .iter()
            .filter_map(|t| t.last_activity.map(|at| (t.uuid.as_str(), at)))
            .collect();
        let now = SystemTime::now();
        for tab in &mut self.tabs {
            tab.last_activity
                .set(seed_activity(seeds.get(tab.uuid.as_str()).copied(), now));
        }
        // The seeded ages must be on the labels from the first paint, not 30 s
        // later.
        self.refresh_ages();

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
        // Remote tabs came back detached, behind their host page. Each host
        // logs in once now; its tabs spawn only after its list-sessions
        // succeeded (on_host_connected).
        for host in state::remote_hosts(&saved) {
            self.connect_host(&host);
        }
        // Same reasoning as the browsers below: filled before the save, so the
        // state written below still names the owner while the restore waits.
        self.pending_android_restore = state::android_restore_target(&saved);
        // Before the save, not after: this fills `pending_browser_restore`, so
        // the state written below already records every restored group as
        // browser_open (see `browser_open_desired`). Reversing the order would
        // persist browser_open=false for all of them until the first
        // RestoreBrowser save.
        self.start_browser_maintenance();
        self.queue_android_restore();
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
                browser_split: if self.attached_host == Some(g.id) {
                    live_split
                } else {
                    g.browser_split
                },
                default_url: g.default_url.clone(),
                host: g.host.clone(),
                overview_root: g.overview_root.clone(),
                overview_mode: g.overview_mode,
            })
            .collect();
        let tabs = self
            .tabs
            .iter()
            .map(|t| SavedTab {
                uuid: t.uuid.clone(),
                group: t.group,
                title: t.title.clone(),
                // Unix seconds, and never a panic on a clock: a pre-epoch
                // stamp (only reachable via a wild clock jump) is dropped
                // rather than unwrapped, which reseeds the tab to `now` on
                // the next start.
                last_activity: t
                    .last_activity
                    .get()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs()),
                // The dedupe anchor: what `update_title` will compare the
                // first post-restart title against.
                last_title: Some(t.title.clone()),
                claude: t.claude.clone(),
            })
            .collect();
        let active = self
            .active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
            .map(|t| t.uuid.clone());
        // The DESIRED owner, like `browser_open`: a restore still queued has
        // no `Android` yet but must still be written.
        let live_owner = self
            .groups
            .iter()
            .find(|g| g.android.is_some())
            .map(|g| g.uuid.as_str());
        let pending_owner = self
            .pending_android_restore
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .map(|g| g.uuid.as_str());
        let state = SavedState {
            groups,
            tabs,
            active,
            sidebar_visible: self.sidebar_visible,
            linger_warning_dismissed: self.linger_dismissed,
            sidebar_order: self.sidebar_order,
            pending_kills: self.pending_kills.clone(),
            android_owner: state::android_owner_desired(live_owner, pending_owner),
        };
        self.persist(&state);
        // Same choke point as the save, so the CLI always sees what was last
        // written rather than a separately maintained copy.
        self.publish_groups();
    }

    /// Publish the groups for the CLI: on every save, and on `CdpReady`/
    /// `AndroidReady`, which change nothing the state file records but are
    /// exactly what `kabelsalat browser`/`kabelsalat android` ask about. The
    /// adb serial is published only while the pane reported ready and still
    /// runs: an `Android` torn down at shutdown stays on its group.
    fn publish_groups(&self) {
        control::publish(
            self.groups
                .iter()
                .map(|g| crate::cli::GroupInfo {
                    uuid: g.uuid.clone(),
                    name: g.name.clone(),
                    tabs: self.tabs.iter().filter(|t| t.group == g.id).count(),
                    host: g.host.clone(),
                    cdp: g.browser.as_ref().and_then(|b| b.cdp_url()),
                    android: g.android.as_ref().map(|a| crate::cli::AndroidInfo {
                        ctl: a.control_socket_path().to_path_buf(),
                        adb: a.adb().filter(|_| a.is_running()).map(str::to_string),
                    }),
                    overview_root: g.overview_root.clone(),
                    issues: g
                        .overview
                        .as_ref()
                        .map(|data| {
                            data.issues
                                .iter()
                                .map(|issue| crate::cli::IssueLine {
                                    kind: issue.kind.label().to_string(),
                                    nodes: issue.nodes.clone(),
                                    file: issue.file.clone(),
                                    detail: issue.detail.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
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

    // ---- remote hosts ---------------------------------------------------

    fn host_tab_ids(&self, host: &str) -> Vec<usize> {
        self.tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.id)
            .collect()
    }

    /// A remote tab's ssh client exited. Status 255 is ssh itself failing:
    /// the worker checks the master (`ssh -O check`), and a dead master takes
    /// the whole host to Disconnected. Any other status: the worker lists the
    /// host's sessions — asynchronously, never on this thread — and
    /// `on_tab_sessions` closes or reattaches.
    fn remote_child_exited(&mut self, id: usize, status: i32) {
        let Some(host) = self.tab_host(id) else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        tab.attached = false;
        if self.host_state(&host) != HostState::Live {
            // Already down or reconnecting; the next connect reattaches.
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                self.refresh_tab_page(tab);
            }
            return;
        }
        let ssh_failed = decode_exit(status) == remote::SSH_FAILED;
        if let Some(worker) = self.worker(&host) {
            worker.child_exited(id, ssh_failed);
        }
    }

    /// The listing a remote child exit asked for. Only a successful listing
    /// without the session closes the tab (`session_definitively_gone`); an
    /// error means liveness is unknown and the tab stays. A dead pane marks
    /// the tab crashed (remote servers have no pane-died file hook).
    fn on_tab_sessions(&mut self, id: usize, result: Result<Vec<SessionInfo>, String>) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        // Attached again since the exit asked for this listing (a Reconnect
        // or a host connect ran `attach_host_tabs` in the meantime): the
        // listing is stale. Acting on it would spawn a second client into
        // the terminal, or close the tab and kill the session `-A` just made.
        if tab.attached {
            return;
        }
        let uuid = tab.uuid.clone();
        let since_spawn = tab.spawned_at.map(|at| at.elapsed());
        let Some(host) = self.group_host(tab.group) else {
            return;
        };
        let result = result.map_err(TmuxError::Command);
        if session_definitively_gone(&result, &uuid) {
            self.close_tab(id);
            return;
        }
        let dead_exit = result
            .ok()
            .and_then(|sessions| sessions.into_iter().find(|s| s.uuid == uuid))
            .filter(|s| s.pane_dead)
            .map(|s| s.dead_status.unwrap_or(-1));
        if let Some(code) = dead_exit
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id)
        {
            tab.crashed = Some(code);
        }
        if self.host_state(&host) == HostState::Live && reattach_allowed(since_spawn) {
            self.spawn_remote_tab(id);
        } else if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
            if self.host_state(&host) == HostState::Live {
                eprintln!(
                    "kabelsalat: {uuid} on {host} exited right after attaching; \
                     not reattaching automatically"
                );
            }
            // Shows the Reconnect page; its button reattaches this tab.
            self.refresh_tab_page(tab);
        }
        self.rebuild_list();
    }

    /// Directory for a new tab in `group`, taken from the active tab when
    /// both run on the same machine. Local: `active_tab_cwd` as before.
    /// Remote: the host's `pane_current_path`, waited on for at most
    /// `REMOTE_CWD_WAIT` and never checked against the local filesystem;
    /// `None` starts the shell in the remote home.
    fn new_tab_cwd(&self, group: usize) -> Option<PathBuf> {
        let tab = self.active_tab()?;
        let host = self.group_host(group);
        if self.group_host(tab.group) != host {
            return None;
        }
        match host {
            None => self.active_tab_cwd(),
            Some(host) => {
                if self.host_state(&host) != HostState::Live {
                    return None;
                }
                self.worker(&host)?
                    .pane_current_path(&tab.uuid, REMOTE_CWD_WAIT)
                    .map(PathBuf::from)
            }
        }
    }

    fn tab_host(&self, id: usize) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| self.group_host(t.group))
    }

    /// A host nobody has connected yet reads as connecting: its tabs are
    /// about to be, and must not look broken in the meantime.
    fn host_state(&self, host: &str) -> HostState {
        self.hosts
            .get(host)
            .map_or(HostState::Connecting, |h| h.state.clone())
    }

    fn worker(&self, host: &str) -> Option<&RemoteWorker> {
        self.hosts
            .get(host)
            .and_then(|h| h.link.as_ref())
            .map(|link| &link.worker)
    }

    fn set_host_state(&mut self, host: &str, state: HostState) {
        let entry = self
            .hosts
            .entry(host.to_string())
            .or_insert_with(|| RemoteHost {
                state: HostState::Connecting,
                link: None,
                retry: Retry::default(),
                discovery: Discovery::default(),
            });
        entry.state = state;
        // Whatever was in flight on the old state is moot; the host's own
        // answer, if it ever comes, is harmless.
        entry.discovery = Discovery::default();
        self.refresh_host_pages(host);
        self.rebuild_list();
    }

    /// Log in to `host` (once; the worker runs the host check and lists the
    /// sessions), or, when it is already live, reattach its detached tabs.
    /// This login may prompt; background retries (`retry_hosts`) never do.
    fn connect_host(&mut self, host: &str) {
        // Asked for explicitly (or at startup): retries start over.
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.retry.failures = 0;
            entry.retry.due = None;
        }
        match self.hosts.get(host).map(|h| h.state.clone()) {
            // One login at a time; the running attempt reports back.
            Some(HostState::Connecting) if self.worker(host).is_some() => return,
            Some(HostState::Live) => {
                self.attach_host_tabs(host);
                return;
            }
            _ => {}
        }
        if !self.ssh.is_available() {
            self.set_host_state(host, HostState::Disconnected(RemoteError::SshUnsupported));
            return;
        }
        // The dialog validates, but a hand-edited state file does not: a
        // destination starting with '-' would reach ssh as an option.
        if let Err(err) = state::validate_host(host) {
            self.set_host_state(
                host,
                HostState::Disconnected(RemoteError::Other(err.to_string())),
            );
            return;
        }
        if self.worker(host).is_none() {
            match self.spawn_worker(host) {
                Ok(link) => {
                    self.hosts
                        .entry(host.to_string())
                        .or_insert_with(|| RemoteHost {
                            state: HostState::Connecting,
                            link: None,
                            retry: Retry::default(),
                            discovery: Discovery::default(),
                        })
                        .link = Some(link);
                }
                Err(detail) => {
                    self.set_host_state(host, HostState::Disconnected(RemoteError::Other(detail)));
                    return;
                }
            }
        }
        self.set_host_state(host, HostState::Connecting);
        if let Some(worker) = self.worker(host) {
            worker.connect();
        }
    }

    fn spawn_worker(&self, host: &str) -> Result<HostLink, String> {
        let control_path = remote_worker::control_path(host)
            .map_err(|err| format!("no directory for the ssh control socket: {err}"))?;
        let input = self.input.clone();
        let reply_host = host.to_string();
        let worker = RemoteWorker::spawn(
            host.to_string(),
            control_path.clone(),
            self.auth.clone(),
            SshRunner,
            move |event| {
                let _ = input.send(Msg::Remote {
                    host: reply_host.clone(),
                    event,
                });
            },
        )
        .map_err(|err| format!("could not start the connection thread: {err}"))?;
        Ok(HostLink {
            ctl: TmuxCtl::remote(host, &control_path),
            worker,
        })
    }

    fn on_remote_event(&mut self, host: String, event: RemoteEvent) {
        match event {
            RemoteEvent::Connected(sessions) => self.on_host_connected(&host, &sessions),
            RemoteEvent::ConnectFailed(err) => {
                eprintln!("kabelsalat: {}", err.message(&host));
                self.set_host_state(&host, HostState::Disconnected(err));
            }
            RemoteEvent::RetryFailed(err) => self.on_retry_failed(&host, err),
            RemoteEvent::Killed(uuids) => self
                .pending_kills
                .retain(|kill| kill.host != host || !uuids.contains(&kill.uuid)),
            RemoteEvent::MasterDead => {
                // Only a live host can die; a Reconnect already underway
                // (Connecting) must not be overridden by a stale report.
                if self.host_state(&host) != HostState::Live {
                    return;
                }
                eprintln!("kabelsalat: lost the connection to {host}");
                // Every client of the master went down with it. No automatic
                // reattach: Reconnect reruns the login, then attaches them.
                for id in self.host_tab_ids(&host) {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.attached = false;
                    }
                }
                self.set_host_state(&host, HostState::Disconnected(RemoteError::Unreachable));
            }
            RemoteEvent::TabSessions { tab, result } => self.on_tab_sessions(tab, result),
            // Taken off the message before it gets here.
            RemoteEvent::ClaudeSessions(_) => {}
        }
    }

    /// A host answered a discovery: apply it to its tabs with the same rule
    /// as the local tick. Returns whether any tab changed.
    fn on_claude_sessions(&mut self, host: &str, found: HashMap<String, ClaudeSession>) -> bool {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.discovery.running = false;
            entry.discovery.next = Some(Instant::now() + Duration::from_secs(REMOTE_CLAUDE_SECS));
        }
        let mut changed = false;
        for id in self.host_tab_ids(host) {
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                let seen = found.get(&tab.uuid).cloned();
                let now = claude::after_tick(tab.claude.clone(), seen, tab.crashed.is_some());
                if tab.claude != now {
                    tab.claude = now;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Queue a discovery on every live host that is due and has none in
    /// flight.
    fn discover_remote_claude(&mut self) {
        let now = Instant::now();
        for entry in self.hosts.values_mut() {
            if entry.state != HostState::Live || entry.discovery.running {
                continue;
            }
            let Some(link) = &entry.link else {
                continue;
            };
            if entry.discovery.next.is_some_and(|next| now < next) {
                continue;
            }
            entry.discovery.running = true;
            link.worker.discover_claude();
        }
    }

    /// The host passed its check and listed its sessions: plan its tabs
    /// (state::reconcile_remote), flush the queued kills, mark crashed panes,
    /// and only now spawn its tabs.
    fn on_host_connected(&mut self, host: &str, sessions: &[SessionInfo]) {
        let listing = state::RemoteListing {
            live: sessions.iter().map(|s| s.uuid.clone()).collect(),
            dead: sessions
                .iter()
                .filter(|s| s.pane_dead)
                .map(|s| state::DeadPane {
                    uuid: s.uuid.clone(),
                    exit_code: s.dead_status.unwrap_or(-1),
                })
                .collect(),
        };
        let tabs: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.uuid.clone())
            .collect();
        let queued: Vec<String> = self
            .pending_kills
            .iter()
            .filter(|kill| kill.host == host)
            .map(|kill| kill.uuid.clone())
            .collect();
        let plan = state::reconcile_remote(&tabs, Some(&listing), &queued);
        // A queued kill whose session is already gone is done; the live ones
        // leave the queue when the host confirms them (RemoteEvent::Killed).
        self.pending_kills
            .retain(|kill| kill.host != host || plan.kill.contains(&kill.uuid));
        if !plan.kill.is_empty()
            && let Some(worker) = self.worker(host)
        {
            worker.kill(plan.kill.clone());
        }
        for attach in &plan.attach {
            if let Some(code) = attach.dead_exit
                && let Some(tab) = self.tabs.iter_mut().find(|t| t.uuid == attach.uuid)
            {
                tab.crashed = Some(code);
            }
        }
        // A tab whose session did not survive on the host comes back as the
        // claude it had, resumed there; a tab with a first spawn still
        // pending (created while the host was down) keeps that instead.
        for tab in self.tabs.iter_mut().filter(|t| tabs.contains(&t.uuid)) {
            let resume = state::resume_on_restore(tab.claude.as_ref(), &tab.uuid, &listing.live);
            if let Some(session) = resume
                && tab.pending.as_ref().is_none_or(|p| p.command.is_none())
            {
                tab.pending = Some(PendingSpawn {
                    cwd: Some(session.cwd.clone()),
                    command: Some(session.resume_argv()),
                });
            }
        }
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.state = HostState::Live;
            // A retry may still be queued behind this connect; it reports
            // Connected too, which is harmless.
            entry.retry.failures = 0;
            entry.retry.due = None;
            // A fresh host is inspected by the next tick.
            entry.discovery = Discovery::default();
        }
        self.attach_host_tabs(host);
        self.refresh_host_pages(host);
        self.rebuild_list();
    }

    /// Start a background retry for every disconnected host that is due. It
    /// logs in with `BatchMode=yes`, so it reconnects only where no prompt is
    /// needed (a key or an agent) and never pops up askpass; success goes
    /// through `on_host_connected` exactly like Reconnect.
    fn retry_hosts(&mut self) {
        let now = Instant::now();
        for entry in self.hosts.values_mut() {
            let HostState::Disconnected(err) = &entry.state else {
                continue;
            };
            let Some(link) = &entry.link else {
                continue;
            };
            if entry.retry.running || !err.retry_allowed() {
                continue;
            }
            let failures = entry.retry.failures;
            let due = *entry
                .retry
                .due
                .get_or_insert_with(|| now + remote::retry_delay(failures));
            if now < due {
                continue;
            }
            entry.retry.due = None;
            entry.retry.running = true;
            link.worker.retry();
        }
    }

    /// A background retry failed. The host may have moved on meanwhile (a
    /// Reconnect is underway, or it came back): then only the bookkeeping
    /// changes. Otherwise the page shows the latest reason, and a reason a
    /// person must deal with ends the retries (`retry_allowed`).
    fn on_retry_failed(&mut self, host: &str, err: RemoteError) {
        let Some(entry) = self.hosts.get_mut(host) else {
            return;
        };
        entry.retry.running = false;
        entry.retry.failures = entry.retry.failures.saturating_add(1);
        if matches!(&entry.state, HostState::Disconnected(current) if *current != err) {
            self.set_host_state(host, HostState::Disconnected(err));
        }
    }

    /// Whether `host` waits for a background retry to bring it back.
    fn retries_on_its_own(&self, host: &str) -> bool {
        self.hosts.get(host).is_some_and(|entry| {
            entry.link.is_some()
                && matches!(&entry.state, HostState::Disconnected(err) if err.retry_allowed())
        })
    }

    /// Spawn the client of every detached tab on a live host: restored and
    /// fresh tabs alike (`new-session -A` attaches or creates).
    fn attach_host_tabs(&mut self, host: &str) {
        let detached: Vec<usize> = self
            .tabs
            .iter()
            .filter(|t| !t.attached && self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.id)
            .collect();
        for id in detached {
            self.spawn_remote_tab(id);
        }
    }

    /// Run a remote tab's `ssh -S … -t <dest> -- tmux … new-session -A …`.
    /// Only ever called while its host is live. KABELSALAT_* stay local, so
    /// no `-e` pairs.
    fn spawn_remote_tab(&mut self, id: usize) {
        let Some(host) = self.tab_host(id) else {
            return;
        };
        let Some(ctl) = self
            .hosts
            .get(&host)
            .and_then(|h| h.link.as_ref())
            .map(|link| link.ctl.clone())
        else {
            return;
        };
        let active = self.active == Some(id);
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let pending = tab.pending.take().unwrap_or_default();
        let argv = ctl.spawn_argv(
            &tab.uuid,
            pending.cwd.as_deref(),
            pending.command.as_deref(),
            &[],
        );
        // A reconnect re-attaches a tab that may have been armed long ago;
        // its attach repaint must not count as activity.
        tab.armed.respawn();
        spawn_client(&tab.terminal, &argv);
        tab.attached = true;
        tab.spawned_at = Some(Instant::now());
        tab.view.set_visible_child_name("terminal");
        if active {
            tab.terminal.grab_focus();
        }
    }

    fn refresh_host_pages(&self, host: &str) {
        for tab in self
            .tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
        {
            self.refresh_tab_page(tab);
        }
    }

    /// Show a remote tab's terminal or its host page, whichever its host's
    /// state and its own attachment call for. Local tabs: no-op.
    fn refresh_tab_page(&self, tab: &Tab) {
        let Some(page) = &tab.status else {
            return;
        };
        let Some(host) = self.group_host(tab.group) else {
            return;
        };
        match self.host_state(&host) {
            HostState::Live if tab.attached => {
                tab.view.set_visible_child_name("terminal");
                return;
            }
            HostState::Live => page.show(
                &host,
                "This tab is not attached to its session. Reconnect to attach it again.",
                false,
                true,
            ),
            HostState::Connecting => page.show(&format!("Connecting to {host}…"), "", true, false),
            HostState::Disconnected(err) => {
                let mut description = err.message(&host);
                if self.retries_on_its_own(&host) {
                    description.push_str(
                        "\n\nkabelsalat reconnects on its own as soon as the host is reachable again.",
                    );
                }
                page.show(
                    &format!("{host} is disconnected"),
                    &description,
                    false,
                    true,
                );
            }
        }
        tab.view.set_visible_child_name("status");
    }

    // ---- browser pane ---------------------------------------------------

    /// Does the active group have a browser at all? Drives the overflow menu.
    /// Tooltip of the "New remote group" button: its shortcut, or why it is
    /// insensitive.
    fn remote_button_tooltip(&self) -> String {
        self.ssh
            .reason()
            .unwrap_or_else(|| "New remote group (Ctrl+Shift+H)".to_string())
    }

    fn active_group_has_browser(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.browser.is_some())
    }

    /// Does the active group have panes that are currently hidden? Drives
    /// the header-bar indicator.
    fn active_panes_hidden(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.has_panes() && !g.panes_visible)
    }

    /// Can the active group's front pane be captured right now? Drives the
    /// screenshot button. The pane's compositor renders the frame, so a pane
    /// whose compositor died has nothing left to show — and a hidden one has,
    /// which is why this asks "running", not "visible".
    fn active_front_running(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| match g.front_pane {
                PaneKind::Browser => g.browser.as_ref().is_some_and(Browser::is_running),
                PaneKind::Android => g.android.as_ref().is_some_and(Android::is_running),
            })
    }

    /// Is Android up anywhere? Drives "Stop Android".
    fn android_running(&self) -> bool {
        self.groups.iter().any(|g| g.android.is_some())
    }

    /// Ask the active group's front pane for a fresh frame. The pane answers
    /// on the GTK main context, so the reply comes back as an ordinary
    /// message — carrying the tab the user asked from, because the answer
    /// may take seconds and the user is free to switch tabs meanwhile.
    fn capture_front_pane(&self, sender: &ComponentSender<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let tab_id = tab.id;
        let Some(group) = self.groups.iter().find(|g| g.id == tab.group) else {
            return;
        };
        let sender = sender.clone();
        let callback = move |result: Result<CapturedFrame, browser::CaptureError>| {
            sender.input(Msg::ScreenshotCaptured(
                tab_id,
                result.map_err(|err| err.to_string()),
            ));
        };
        match (group.front_pane, &group.browser, &group.android) {
            (PaneKind::Browser, Some(browser), _) => browser.capture_frame(callback),
            (PaneKind::Android, _, Some(android)) => android.capture_frame(callback),
            _ => {}
        }
    }

    /// Put a captured frame on the clipboard, then press Ctrl-V in the tab the
    /// capture was started from so a `claude` running there stages it as an
    /// image.
    ///
    /// The claim outlives the paste — GDK serializes the texture per requester
    /// — so the screenshot stays available for the user to paste anywhere else.
    /// The keystroke is deferred by one main-loop iteration because the claim
    /// only reaches the compositor when the loop next runs, and the reader in
    /// the tab must not beat it there.
    ///
    /// The keystroke only goes to `tab`, and only while it is still the active
    /// one: a frame that arrives after the user moved on would otherwise be
    /// typed into an unrelated agent's session. The clipboard is set either
    /// way, so a skipped paste still leaves the screenshot in hand.
    fn paste_screenshot(&self, tab: usize, frame: CapturedFrame) {
        if !frame_backs_texture(frame.width, frame.height, frame.stride, frame.rgba.len()) {
            eprintln!("kabelsalat: unusable screenshot frame: {frame:?}");
            self.show_toast("The screenshot failed.");
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

        let Some(tab) = self.active_tab().filter(|t| t.id == tab) else {
            self.show_toast("Screenshot copied to the clipboard.");
            return;
        };
        let terminal = tab.terminal.clone();
        gtk::glib::idle_add_local_once(move || {
            terminal.feed_child(&[CTRL_V]);
            // The click left the keyboard where it was; the paste is an
            // invitation to type next to the image, so hand it to the
            // terminal that just received it.
            terminal.grab_focus();
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

    /// Make the split show exactly the active group's pane area, if it has a
    /// visible one, with its front pane selected. The outgoing host's divider
    /// position is saved and the host is unparented — its panes are unmapped,
    /// which pauses their frame pumps, and keep running, so coming back to
    /// the group is instant with state intact.
    fn sync_pane_host(&mut self) {
        // Before anything else: the active group may have changed even when
        // the attached host does not (e.g. to a group whose panes are hidden),
        // and the CDP menu must reflect it regardless.
        self.refresh_cdp_menu();
        let want = self.active_group().filter(|id| {
            self.groups
                .iter()
                .any(|g| g.id == *id && g.has_panes() && g.panes_visible)
        });
        if self.attached_host != want {
            if let Some(previous) = self.attached_host.take() {
                let split = self.browser_paned.position() as f64;
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == previous) {
                    group.browser_split = split;
                }
                self.browser_paned.set_end_child(gtk::Widget::NONE);
            }
            if let Some(id) = want
                && let Some(group) = self.groups.iter().find(|g| g.id == id)
            {
                let split = group.browser_split as i32;
                let widget: gtk::Widget = group.panes.root.clone().upcast();
                self.browser_paned.set_end_child(Some(&widget));
                self.apply_browser_split(split, widget);
                self.attached_host = Some(id);
            }
        }
        // Every host shows its front pane, attached or not, so coming back to
        // a group lands on the right tab without a flash of the other one.
        for group in &self.groups {
            group.panes.select(group.front_pane);
        }
        // Every switch path funnels through here, so this is where the new
        // active group's overview mode is restored.
        self.sync_overview();
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
    /// front pane is an explicit request to work in it, so it gets the keyboard;
    /// hiding it gives the keyboard back to the terminal.
    fn focus_panes_or_terminal(&self) {
        let front = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .filter(|g| g.panes_visible)
            .and_then(Group::front_widget);
        if let Some(widget) = front {
            widget.grab_focus();
        } else if let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
        {
            tab.terminal.grab_focus();
        }
    }

    /// Alt-2: a visible area → hide it, whichever pane is in front; a hidden
    /// one → show it with the browser in front, spawning one if there is
    /// none (see `state::browser_key_step`). Never panics: a failure to
    /// spawn leaves the group as it was and reports the reason.
    fn toggle_browser(&mut self) {
        let Some(id) = self.active_group() else {
            return;
        };
        let Some(group) = self.groups.iter().find(|g| g.id == id) else {
            return;
        };
        match state::browser_key_step(group.browser.is_some(), group.panes_visible) {
            BrowserKeyStep::Hide => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.panes_visible = false;
                }
            }
            BrowserKeyStep::Front => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    // The browser is open: the key brings it to the front.
                    group.front_pane = PaneKind::Browser;
                    group.panes_visible = true;
                }
            }
            BrowserKeyStep::Spawn => {
                // Keyed by the group's uuid: ids are reused, so an id-keyed
                // profile could be one the startup sweep is still deleting.
                let uuid = group.uuid.clone();
                // Cloned like the uuid, so no borrow of `self.groups` is alive
                // across the spawn.
                let default_url = group.default_url.clone();
                match Browser::spawn(&uuid, &self.state_dir, default_url.as_deref()) {
                    Ok(browser) => {
                        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                            group.add_pane(PaneKind::Browser, browser.widget().upcast_ref(), false);
                            group.browser = Some(browser);
                            group.panes_visible = true;
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
        }
        self.sync_pane_host();
        self.focus_panes_or_terminal();
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
        let had_focus = pane_has_focus(browser.widget().upcast_ref());
        // Unparent first, so the compositor is torn down outside the layout.
        group.panes.remove(PaneKind::Browser);
        let other_open = group.has_panes();
        group.front_pane =
            state::front_after_close(group.front_pane, PaneKind::Browser, other_open);
        if !other_open {
            group.panes_visible = false;
        }
        // NLL: `group` is done; &self calls are fine from here. Runs only
        // when a browser was actually present, so a duplicate BrowserDied
        // (see the doc comment above) stays a true no-op.
        self.cdp_env_unset(id);
        self.sync_pane_host();
        browser.teardown(disposition);
        drop(browser);
        self.refocus_after_close(id, had_focus);
    }

    /// After a pane of group `id` closed: if it held the keyboard, hand it to
    /// what is in front now (another pane or the terminal), so nothing is
    /// left focused-on-nothing. A pane that did not have focus moves nothing.
    fn refocus_after_close(&self, id: usize, had_focus: bool) {
        if had_focus && self.active_group() == Some(id) {
            self.focus_panes_or_terminal();
        }
    }

    /// Remember the divider position of the split the user just dragged, on
    /// the group it belongs to. Persisting happens with the next layout
    /// mutation or at shutdown, not per drag step.
    fn on_browser_split_changed(&mut self) {
        let Some(id) = self.attached_host else {
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
        // The browser is local-only: its endpoint is never exported into a
        // remote group's sessions (they live on another server anyway).
        if self.group_host(group_id).is_some() {
            return;
        }
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
        // The browser is local-only: its endpoint is never exported into a
        // remote group's sessions (they live on another server anyway).
        if self.group_host(group_id).is_some() {
            return;
        }
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in CDP_ENV_KEYS {
                if let Err(err) = tmux.unset_environment(&tab.uuid, key) {
                    eprintln!("kabelsalat: unset {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Publish the Android pair to every session in the group, once Android
    /// has booted. Local groups only; one failing session never stops the
    /// others, and never Android.
    fn android_env_set(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        if self.group_host(group_id).is_some() {
            return;
        }
        let Some(pairs) = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_ref())
            .and_then(android_env)
        else {
            return;
        };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for (key, value) in &pairs {
                if let Err(err) = tmux.set_environment(&tab.uuid, key, value) {
                    eprintln!("kabelsalat: set {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Remove the Android pair from every session in the group, so nothing
    /// started later inherits a dead control socket.
    fn android_env_unset(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        if self.group_host(group_id).is_some() {
            return;
        }
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in ANDROID_ENV_KEYS {
                if let Err(err) = tmux.unset_environment(&tab.uuid, key) {
                    eprintln!("kabelsalat: unset {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Make one session's environment describe its group: `KABELSALAT_GROUP`
    /// always (a tab always belongs to some group — never unset), the CDP
    /// pair and the Android pair each set or unset by whether the group has
    /// a live endpoint / a booted Android. Used when a session is
    /// (re)created or reattached and when a tab moves.
    fn session_env_refresh(&self, tab_uuid: &str, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        // KABELSALAT_GROUP and the endpoint pair stay local (spec).
        if self.group_host(group_id).is_some() {
            return;
        }
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
        match group.android.as_ref().and_then(android_env) {
            Some(pairs) => {
                for (key, value) in &pairs {
                    if let Err(err) = tmux.set_environment(tab_uuid, key, value) {
                        eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
                    }
                }
            }
            // As for CDP: a reattached session may carry a stale pair from a
            // previous run, and this is where it is cleared.
            None => {
                for key in ANDROID_ENV_KEYS {
                    if let Err(err) = tmux.unset_environment(tab_uuid, key) {
                        eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
                    }
                }
            }
        }
    }

    /// The env pairs a session of this group is created with (see
    /// `cli::session_env_pairs`). None only for a group id that no longer
    /// exists.
    fn group_env_pairs(&self, group_id: usize) -> Option<Vec<(&'static str, String)>> {
        self.groups.iter().find(|g| g.id == group_id).map(|g| {
            let cdp = g.browser.as_ref().and_then(|b| b.cdp_url());
            let android = g.android.as_ref().and_then(android_env);
            crate::cli::session_env_pairs(
                &g.uuid,
                cdp.as_deref(),
                android
                    .as_ref()
                    .map(|[(_, ctl), (_, adb)]| (ctl.as_str(), adb.as_str())),
            )
        })
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

    /// Reap exited Chromium processes and Waydroid sessions; each comes back
    /// as `BrowserDied` / `AndroidDied`.
    fn poll_browsers(&mut self) {
        let mut dead = Vec::new();
        let mut android_dead = Vec::new();
        for group in self.groups.iter_mut() {
            let id = group.id;
            if let Some(browser) = &mut group.browser {
                match browser.has_exited() {
                    Ok(true) => dead.push(id),
                    Ok(false) => {}
                    Err(err) => eprintln!("kabelsalat: browser {id} could not be polled: {err}"),
                }
            }
            if let Some(android) = &mut group.android {
                match android.has_exited() {
                    Ok(exited) => {
                        if android::is_dead(exited, android.is_running()) {
                            let reason = if exited {
                                "the Waydroid session exited."
                            } else {
                                "the Android pane's compositor stopped."
                            };
                            android_dead.push((id, android.boot_id(), reason));
                        }
                    }
                    Err(err) => {
                        eprintln!("kabelsalat: Android in group {id} could not be polled: {err}")
                    }
                }
            }
        }
        for id in dead {
            let _ = self.input.send(Msg::BrowserDied(id));
        }
        for (id, boot, reason) in android_dead {
            let _ = self
                .input
                .send(Msg::AndroidDied(id, boot, reason.to_string()));
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
            .filter(|g| g.panes_visible && g.browser.is_none())
            .map(|g| g.id)
            .collect();
        // The browser the user can actually see becomes usable first.
        pending.sort_by_key(|id| Some(*id) != active);
        // Only the active group's browser is shown; the rest come back hidden
        // and announce themselves with the header-bar icon.
        for group in self.groups.iter_mut() {
            if Some(group.id) != active {
                group.panes_visible = false;
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
                        group.add_pane(PaneKind::Browser, browser.widget().upcast_ref(), true);
                        group.browser = Some(browser);
                    }
                    self.start_browser_poll();
                    self.start_cdp_poll(id);
                }
                Err(err) => {
                    eprintln!("kabelsalat: restoring browser for group {id}: {err}");
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id)
                        && !group.has_panes()
                    {
                        group.panes_visible = false;
                    }
                    // One dialog, however many groups fail for the same reason.
                    if !self.browser_restore_error_shown {
                        self.browser_restore_error_shown = true;
                        self.show_notice(&err.to_string());
                    }
                }
            }
        }
        self.sync_pane_host();
        self.queue_browser_restore();
    }

    /// Ask for the pending Android restore on an idle callback, so the window
    /// is interactive first.
    fn queue_android_restore(&self) {
        let Some(id) = self.pending_android_restore else {
            return;
        };
        let input = self.input.clone();
        gtk::glib::idle_add_local_once(move || {
            let _ = input.send(Msg::RestoreAndroid(id));
        });
    }

    /// Bring a restored group's Android back, hidden: the area's visibility
    /// is left as restored, and `spawn_android` adds the page quietly, so
    /// the front pane only changes when Android is the group's only pane. A
    /// group that vanished or got an Android meanwhile is skipped, and so,
    /// with a log line only, is one whose Android another group took
    /// meanwhile. A failure is reported once and forgets the owner, so a
    /// broken Waydroid does not nag at every start.
    fn restore_android(&mut self, id: usize) {
        if self.pending_android_restore != Some(id) {
            return;
        }
        self.pending_android_restore = None;
        if !self
            .groups
            .iter()
            .any(|g| g.id == id && g.android.is_none())
        {
            return;
        }
        if let Some(owner) = self.groups.iter().find(|g| g.android.is_some()) {
            eprintln!(
                "kabelsalat: not restoring Android for group {id}: group {} has it",
                owner.id
            );
            return;
        }
        if let Err(message) = self.spawn_android(id) {
            eprintln!("kabelsalat: restoring Android for group {id}: {message}");
            self.show_notice(&message);
        }
        self.sync_pane_host();
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

    /// Re-read lingering from the system — it may have been changed with
    /// `loginctl` outside the app — then show the dialog for that state:
    /// the offer with its honest downsides while it is off, the fact and a
    /// Disable button once it is on. The responses come from
    /// `autostart::linger_responses`, so the layout is tested there.
    fn show_linger_warning(&mut self) {
        self.refresh_linger();
        let status = self.linger;
        let dialog = adw::AlertDialog::new(
            Some(autostart::linger_dialog_heading(status)),
            Some(&autostart::linger_dialog_body(status)),
        );
        let responses = autostart::linger_responses(status);
        for response in &responses {
            dialog.add_response(response.id(), response.label());
            dialog
                .set_response_appearance(response.id(), response_appearance(response.appearance()));
            if response.appearance() == autostart::Appearance::Suggested {
                dialog.set_default_response(Some(response.id()));
            }
        }
        if let Some(first) = responses.first() {
            dialog.set_close_response(first.id());
        }

        let input = self.input.clone();
        dialog.connect_response(None, move |_, id| {
            let msg = match autostart::LingerResponse::from_id(id) {
                Some(autostart::LingerResponse::Enable) => Msg::SetLinger(true),
                Some(autostart::LingerResponse::Disable) => Msg::SetLinger(false),
                Some(autostart::LingerResponse::Dismiss) => Msg::DismissLingerWarning,
                _ => return,
            };
            let _ = input.send(msg);
        });
        dialog.present(Some(&self.window));
    }

    /// Re-read lingering from the system, and with it the boot-resume state
    /// ("On" versus "At login only" depends on it). Only meaningful with a
    /// usable tmux: without one logout survival is not on offer.
    fn refresh_linger(&mut self) {
        if self.tmux.is_some() {
            self.linger = tmuxctl::detect_linger();
        }
        self.refresh_autostart();
    }

    /// Re-read the boot-resume unit's state from the system.
    fn refresh_autostart(&mut self) {
        self.autostart = autostart::detect(
            self.tmux.is_some() && autostart::has_systemctl(),
            self.linger,
        );
    }

    /// Install or remove the boot-resume unit off the main thread — a user
    /// manager can be slow to answer `systemctl --user` — and report back
    /// as `Msg::AutostartChanged`, with `done` as the toast on success.
    fn autostart_change(&self, change: fn() -> Result<(), String>, done: &'static str) {
        let input = self.input.clone();
        std::thread::spawn(move || {
            let _ = input.send(Msg::AutostartChanged(change().map(|()| done)));
        });
    }

    /// The primary menu's "Resume agent sessions at boot…" dialog. Its text
    /// and responses follow the state (`autostart::responses`); the first
    /// response is the close response, the suggested one the default.
    fn show_autostart_dialog(&self) {
        let path = autostart::config_home()
            .map(|home| autostart::unit_path(&home))
            .unwrap_or_else(|| PathBuf::from(autostart::UNIT_NAME));
        let dialog = adw::AlertDialog::new(
            Some(autostart::dialog_heading()),
            Some(&autostart::dialog_body(self.autostart, &path)),
        );
        let responses = autostart::responses(self.autostart);
        for response in &responses {
            dialog.add_response(response.id(), response.label());
            dialog
                .set_response_appearance(response.id(), response_appearance(response.appearance()));
            if response.appearance() == autostart::Appearance::Suggested {
                dialog.set_default_response(Some(response.id()));
            }
        }
        if let Some(first) = responses.first() {
            dialog.set_close_response(first.id());
        }
        let input = self.input.clone();
        dialog.connect_response(None, move |_, id| {
            let msg = match autostart::Response::from_id(id) {
                Some(autostart::Response::Install | autostart::Response::Repair) => {
                    Msg::AutostartInstall
                }
                Some(autostart::Response::Remove) => Msg::AutostartRemove,
                Some(autostart::Response::EnableLinger) => Msg::SetLinger(true),
                _ => return,
            };
            let _ = input.send(msg);
        });
        dialog.present(Some(&self.window));
    }

    /// `kabelsalat browser`: bring a group's browser up the way a restore
    /// does — hidden unless the group is the active one — and never focus
    /// it. A group that already has one, or that is remote (the CLI refused
    /// it against a snapshot copy), is left alone.
    fn open_browser(&mut self, group_uuid: &str) {
        let Some(id) = self
            .groups
            .iter()
            .find(|g| g.uuid == group_uuid)
            .map(|g| g.id)
        else {
            eprintln!("browser request for unknown group {group_uuid}");
            return;
        };
        if self.group_host(id).is_some() {
            return;
        }
        let Some((uuid, default_url)) = self
            .groups
            .iter()
            .find(|g| g.id == id && g.browser.is_none())
            .map(|g| (g.uuid.clone(), g.default_url.clone()))
        else {
            return;
        };
        let active = self.active_group() == Some(id);
        match Browser::spawn(&uuid, &self.state_dir, default_url.as_deref()) {
            Ok(browser) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    // Quiet: the front pane and the area only change when this
                    // is the group's first pane (see the state.rs tests).
                    let other_open =
                        group.add_pane(PaneKind::Browser, browser.widget().upcast_ref(), true);
                    group.panes_visible =
                        state::quiet_open_visible(group.panes_visible, active, other_open);
                    group.browser = Some(browser);
                }
                self.start_browser_poll();
                self.start_cdp_poll(id);
            }
            Err(err) => {
                eprintln!("kabelsalat: opening the browser of group {id}: {err}");
                self.show_notice(&err.to_string());
                return;
            }
        }
        self.sync_pane_host();
    }

    /// A group's name for messages, or its uuid when it has none.
    fn group_label(&self, uuid: &str) -> String {
        self.groups
            .iter()
            .find(|g| g.uuid == uuid && !g.name.is_empty())
            .map_or_else(|| uuid.to_string(), |g| g.name.clone())
    }

    /// Bring Android up in group `id` and add its page. Quiet: the front
    /// pane only changes when Android is the group's first pane (see
    /// `Group::add_pane`), and the area's visibility is left alone — the
    /// caller decides those. Already up here is success; up in another
    /// group, or a remote group, is refused.
    fn spawn_android(&mut self, id: usize) -> Result<(), String> {
        if self.group_host(id).is_some() {
            return Err("Android runs on this computer; a remote group cannot host it.".into());
        }
        let Some(requester) = self
            .groups
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.uuid.clone())
        else {
            return Err("That group no longer exists.".into());
        };
        let owner = self
            .groups
            .iter()
            .find(|g| g.android.is_some())
            .map(|g| g.uuid.clone());
        match android::claim(owner.as_deref(), &requester) {
            android::Claim::Mine => return Ok(()),
            android::Claim::Taken(uuid) => {
                return Err(format!(
                    "Android is already open in group “{}”. There is one per machine; \
                     stop it there first (main menu → Stop Android).",
                    self.group_label(&uuid)
                ));
            }
            android::Claim::Free => {}
        }
        let input = self.input.clone();
        let android = Android::spawn(&self.state_dir, move |boot, result| {
            let msg = match result {
                Ok(serial) => Msg::AndroidReady(id, boot, serial),
                Err(reason) => Msg::AndroidDied(id, boot, reason),
            };
            let _ = input.send(msg);
        })
        .map_err(|err| err.to_string())?;
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.add_pane(PaneKind::Android, android.widget().upcast_ref(), true);
            group.android = Some(android);
        }
        // The exit poll reaps Android's session too.
        self.start_browser_poll();
        Ok(())
    }

    /// Alt+3: open Android in the active group, or bring it to the front;
    /// either way the area becomes visible and gets the keyboard.
    fn toggle_android(&mut self) {
        let Some(id) = self.active_group() else {
            return;
        };
        if let Err(message) = self.spawn_android(id) {
            self.show_notice(&message);
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            // Open now, freshly or already: the key brings it to the front.
            group.front_pane = PaneKind::Android;
            group.panes_visible = true;
        }
        self.sync_pane_host();
        self.focus_panes_or_terminal();
    }

    /// `kabelsalat android`: bring Android up quietly — never focus, never
    /// switch groups, never raise. `spawn_android` adds the page quietly (it
    /// becomes the front pane only when it is the group's first); this shows
    /// the area only for the active group's first pane. Refusals (owned
    /// elsewhere since the CLI's snapshot, Waydroid missing) are reported
    /// like `open_browser`'s.
    fn open_android(&mut self, group_uuid: &str) {
        let Some((id, other_open, has_android)) = self
            .groups
            .iter()
            .find(|g| g.uuid == group_uuid)
            .map(|g| (g.id, g.has_panes(), g.android.is_some()))
        else {
            eprintln!("android request for unknown group {group_uuid}");
            return;
        };
        if has_android {
            return;
        }
        let active = self.active_group() == Some(id);
        if let Err(message) = self.spawn_android(id) {
            eprintln!("kabelsalat: opening Android in group {id}: {message}");
            self.show_notice(&message);
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.panes_visible =
                state::quiet_open_visible(group.panes_visible, active, other_open);
        }
        self.sync_pane_host();
    }

    /// "Stop Android" / the tab's close button: wherever it runs.
    fn stop_android(&mut self) {
        if let Some(id) = self
            .groups
            .iter()
            .find(|g| g.android.is_some())
            .map(|g| g.id)
        {
            self.close_android(id);
        }
    }

    /// Tear group `id`'s Android down: unset its env, remove its page, stop
    /// the session, close the pane. A group without one is a no-op.
    fn close_android(&mut self, id: usize) {
        if !self
            .groups
            .iter()
            .any(|g| g.id == id && g.android.is_some())
        {
            return;
        }
        self.android_env_unset(id);
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return;
        };
        let Some(mut android) = group.android.take() else {
            return;
        };
        let had_focus = pane_has_focus(android.widget().upcast_ref());
        group.panes.remove(PaneKind::Android);
        let other_open = group.has_panes();
        group.front_pane =
            state::front_after_close(group.front_pane, PaneKind::Android, other_open);
        if !other_open {
            group.panes_visible = false;
        }
        self.sync_pane_host();
        android.teardown();
        drop(android);
        self.publish_groups();
        self.refocus_after_close(id, had_focus);
    }

    /// The boot thread's success: record the serial, publish the pair. A
    /// report from a pane that was stopped meanwhile is ignored.
    fn android_ready(&mut self, group_id: usize, boot: u64, serial: String) {
        let Some(android) = self
            .groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_mut())
            .filter(|a| a.boot_id() == boot)
        else {
            return;
        };
        android.set_ready(serial);
        self.android_env_set(group_id);
        self.publish_groups();
    }

    /// Boot failure or session exit: tear down like Stop, and say why. A
    /// report from a pane that was stopped meanwhile is ignored.
    fn android_died(&mut self, group_id: usize, boot: u64, reason: &str) {
        let current = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_ref())
            .is_some_and(|a| a.boot_id() == boot);
        if !current {
            return;
        }
        self.close_android(group_id);
        eprintln!("kabelsalat: Android stopped: {reason}");
        self.show_notice(&format!(
            "Android stopped: {reason}\n\nThe Waydroid session log is at {}.",
            android::log_path(&self.state_dir).display()
        ));
    }

    /// Run `loginctl enable-linger` or `disable-linger` and re-check, off the
    /// GTK main thread. Both trigger a polkit action that may prompt for
    /// interactive authentication, which would freeze the UI if run
    /// synchronously here; the work happens on a background thread and its
    /// outcome comes back as `Msg::LingerChanged`.
    fn set_linger(&self, enabled: bool) {
        let input = self.input.clone();
        std::thread::spawn(move || {
            let result = match tmuxctl::set_linger(enabled) {
                Ok(()) => Ok(tmuxctl::detect_linger()),
                Err(err) => Err(err.to_string()),
            };
            let _ = input.send(Msg::LingerChanged(enabled, result));
        });
    }

    /// Apply the outcome of the off-thread `set_linger`. The re-read state
    /// drives the menu caption, the header icon and the boot-resume status
    /// through #[watch]; a toast confirms the change, a notice reports a
    /// failure or a state that did not come out as asked.
    fn on_linger_changed(&mut self, wanted: bool, result: Result<LingerStatus, String>) {
        let verb = if wanted { "enabled" } else { "disabled" };
        match result {
            Ok(status) => {
                self.linger = status;
                // "On" versus "At login only" depends on lingering.
                self.refresh_autostart();
                let expected = if wanted {
                    LingerStatus::Enabled
                } else {
                    LingerStatus::Disabled
                };
                if self.linger == expected {
                    self.show_toast(&format!("Lingering {verb}"));
                } else {
                    self.show_notice(&format!("Lingering could not be confirmed as {verb}."));
                }
            }
            Err(err) => self.show_notice(&format!(
                "{} lingering failed: {err}",
                if wanted { "Enabling" } else { "Disabling" }
            )),
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
        let Some(src_group) = self.tabs.iter().find(|t| t.id == active).map(|t| t.group) else {
            return;
        };
        let src_host = self.group_host(src_group);
        let target = match target {
            Some(id) if self.groups.iter().any(|g| g.id == id) => {
                if !state::drop_allowed(src_host.as_deref(), self.group_host(id).as_deref()) {
                    self.show_toast(CROSS_HOST_TOAST);
                    return;
                }
                id
            }
            Some(_) => return, // group vanished while the picker was open
            // A new group on the tab's own host: a move never changes hosts.
            None => {
                let id = self.create_group();
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.host = src_host;
                }
                id
            }
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
        self.sync_pane_host();
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

        let active_host = self.group_host(current_group);
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
            let text = match &group.host {
                Some(host) if host != name => format!("{name} ({}) · {host}", members.len()),
                _ => format!("{} ({})", name, members.len()),
            };
            let label = gtk::Label::builder()
                .label(text)
                .halign(gtk::Align::Start)
                .margin_start(6)
                .build();
            let row = gtk::ListBoxRow::builder().child(&label).build();
            row.set_widget_name(&group.id.to_string());
            row.add_css_class(group.css);
            let reachable = mode == PickerMode::Jump
                || state::drop_allowed(active_host.as_deref(), group.host.as_deref());
            if reachable {
                first_row.get_or_insert(row.clone());
            } else {
                // Shown, so the list still reads as "all groups", but a tab
                // cannot move to another host.
                row.set_sensitive(false);
                row.set_activatable(false);
                row.set_selectable(false);
                row.set_tooltip_text(Some("On another host: tabs can't move between hosts"));
            }
            list.append(&row);
        }
        if mode == PickerMode::Move {
            let new_label = gtk::Label::builder()
                .label(match &active_host {
                    Some(host) => format!("New group on {host}"),
                    None => "New group".to_string(),
                })
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
        let cwd = self.new_tab_cwd(group);
        let uuid = gtk::glib::uuid_string_random().to_string();
        let id = self.add_tab(uuid, group, None, None, cwd.as_deref(), None, sender);
        self.move_tab_to_group_front(id);
        self.activate(id);
    }

    /// Like `open_tab`, but the tab runs `claude` instead of a shell. Which
    /// session that becomes is learned afterwards by `refresh_claude_sessions`,
    /// the same way as for a claude typed into any shell.
    fn open_claude_tab(&mut self, group: usize, sender: &ComponentSender<Self>) {
        let cwd = self.new_tab_cwd(group);
        let uuid = gtk::glib::uuid_string_random().to_string();
        let command = ["claude".to_string()];
        let id = self.add_tab(
            uuid,
            group,
            Some("claude".into()),
            None,
            cwd.as_deref(),
            Some(&command),
            sender,
        );
        self.move_tab_to_group_front(id);
        self.activate(id);
    }

    /// Match the local tabs against the claude processes running in them and
    /// persist what changed, so a tab that comes back after a reboot resumes
    /// the right session. A tab whose claude is gone forgets it: the shell
    /// is what the tab is now, and what a respawn should give back.
    fn refresh_claude_sessions(&mut self) {
        self.discover_remote_claude();
        self.refresh_local_claude_sessions();
    }

    /// The local half of `refresh_claude_sessions`: the registry and `/proc`
    /// against the local server's panes. Nothing to do without tmux.
    fn refresh_local_claude_sessions(&mut self) {
        let Some(tmux) = &self.tmux else {
            return;
        };
        let Some(dir) = claude::sessions_dir() else {
            return;
        };
        let proc_root = Path::new("/proc");
        let records = claude::live_records(&dir, proc_root);
        // No claude anywhere is the common case; it costs no tmux call.
        let panes = if records.is_empty() {
            Vec::new()
        } else {
            match tmux.pane_pids() {
                Ok(panes) => panes,
                Err(err) => {
                    eprintln!("tmux list-panes failed: {err}");
                    return;
                }
            }
        };
        let found = claude::sessions_by_pane(&records, &panes, &claude::ProcFs(proc_root));
        let remote: HashSet<usize> = self
            .groups
            .iter()
            .filter(|g| g.host.is_some())
            .map(|g| g.id)
            .collect();
        let mut changed = false;
        for tab in self.tabs.iter_mut().filter(|t| !remote.contains(&t.group)) {
            let seen = found.get(&tab.uuid).cloned();
            let now = claude::after_tick(tab.claude.clone(), seen, tab.crashed.is_some());
            if tab.claude != now {
                tab.claude = now;
                changed = true;
            }
        }
        if changed {
            // The tint comes and goes with the session.
            self.rebuild_list();
            self.save_state();
        }
    }

    /// Newest first: `add_tab` appends, which is what restore wants (saved
    /// order is display order). A tab the user just created instead moves
    /// to the top of its group's block so the sidebar sorts by age.
    fn move_tab_to_group_front(&mut self, id: usize) {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let tab = self.tabs.remove(pos);
        let groups: Vec<usize> = self.tabs.iter().map(|t| t.group).collect();
        let at = state::newest_first_index(&groups, tab.group);
        self.tabs.insert(at, tab);
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
        enable_links(&terminal);
        let host = self.group_host(group);
        let pending = match &host {
            // Never spawned here: a remote shell may only be created after a
            // successful list-sessions from its host, and a host that is
            // already live gets the spawn right below, once the tab exists.
            Some(_) => Some(PendingSpawn {
                cwd: cwd.map(Path::to_path_buf),
                command: command.map(<[String]>::to_vec),
            }),
            None => {
                // Stamped at session creation via new-session -e: the group
                // identity always, the endpoint pair when the group's browser
                // already has one. A -A reattach ignores -e; reattached
                // sessions are refreshed explicitly in restore_or_fresh.
                let pairs = self.group_env_pairs(group).unwrap_or_default();
                let env: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command, &env);
                None
            }
        };

        attach_drag_hint(&terminal, sender);
        attach_link_opener(&terminal);
        attach_middle_paste(&terminal);

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let title = title.unwrap_or_else(|| format!("Terminal {id}"));

        // Activity tracking. The key handler fires per keystroke, far too
        // often for relm4 messages, so it does exactly one `Cell::set` and
        // rides the next regular save rather than triggering one.
        let last_activity = Rc::new(Cell::new(SystemTime::now()));
        // Capture phase and non-consuming: VTE still sees every key exactly
        // as before, byte-identical. Only Esc and Enter stamp — see
        // `is_activity_key`.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let last = last_activity.clone();
            let pending = self.resort_pending.clone();
            let input = sender.input_sender().clone();
            move |_, key, _, _| {
                if is_activity_key(key) {
                    last.set(SystemTime::now());
                    request_resort(&pending, &input);
                }
                gtk::glib::Propagation::Proceed
            }
        });
        terminal.add_controller(keys);

        // Output counts too: anything that changes the visible screen stamps
        // the tab, once the attach repaint has settled. The settle window
        // opens on the *first* output, not on construction: the attach is
        // spawned asynchronously and its whole-screen repaint lands well
        // after a wall-clock window started here would have closed (every
        // restored tab would then be stamped "now" and its persisted age
        // lost). Known cost: a TUI that repaints while idle (spinner, clock)
        // keeps its tab fresh.
        // A change of the grid size is not activity either: tmux repaints
        // every pane on a resize (the window settling after launch, a tab
        // first shown, the sidebar toggled), which would stamp every tab at
        // once. It reopens the settle window instead.
        let armed = Rc::new(ActivityArm::new());
        let grid = Cell::new((0, 0));
        terminal.connect_contents_changed({
            let armed = armed.clone();
            let last = last_activity.clone();
            let pending = self.resort_pending.clone();
            let input = sender.input_sender().clone();
            move |terminal| {
                let resized = grid.replace((terminal.row_count(), terminal.column_count()))
                    != (terminal.row_count(), terminal.column_count());
                if let Some(generation) = armed.output() {
                    arm_later(&armed, generation);
                } else if resized {
                    arm_later(&armed, armed.disarm());
                } else if armed.is_armed() {
                    last.set(SystemTime::now());
                    request_resort(&pending, &input);
                }
            }
        });

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

        let view = gtk::Stack::new();
        view.add_named(&terminal, Some("terminal"));
        let status = host.as_deref().map(|host| {
            let page = HostPage::new(host, &self.input);
            view.add_named(&page.page, Some("status"));
            page
        });
        self.stack.add_child(&view);
        self.tabs.push(Tab {
            id,
            uuid,
            group,
            title,
            crashed,
            terminal,
            last_activity,
            armed,
            age_shown: age_prefix(Duration::ZERO),
            view,
            status,
            attached: host.is_none(),
            pending,
            spawned_at: None,
            claude: None,
            tags: None,
            tag_mark: None,
            last_tag_run: None,
        });
        if let Some(host) = &host {
            if let Some(tab) = self.tabs.last() {
                self.refresh_tab_page(tab);
            }
            if self.host_state(host) == HostState::Live {
                self.spawn_remote_tab(id);
            }
        }
        id
    }

    /// Rerun the shell in a crashed tab, clearing its crashed marker. With
    /// tmux the dead pane is respawned in place (client stays attached) —
    /// through the host's worker for a remote tab; in the fallback path a
    /// fresh $SHELL is spawned into the same terminal.
    fn restart_tab(&mut self, id: usize) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let uuid = tab.uuid.clone();
        let terminal = tab.terminal.clone();
        match (self.group_host(tab.group), &self.tmux) {
            (Some(host), _) => {
                // A host that is not live has nothing to respawn into.
                if self.host_state(&host) != HostState::Live {
                    return;
                }
                // As locally: a tab that hosted a claude restarts as that
                // claude, resumed on the host.
                let resume = tab.claude.as_ref().map(|c| c.resume_argv());
                let cwd = tab.claude.as_ref().map(|c| c.cwd.clone());
                if let Some(worker) = self.worker(&host) {
                    worker.respawn(uuid, cwd, resume);
                }
            }
            (None, Some(tmux)) => {
                // A tab that hosted a claude restarts as that claude, resumed:
                // rerunning the original command would start a fresh session.
                let resume = tab.claude.as_ref().map(|c| c.resume_argv());
                let cwd = tab.claude.as_ref().map(|c| c.cwd.as_path());
                if let Err(err) = tmux.respawn_pane(&uuid, cwd, resume.as_deref()) {
                    eprintln!("failed to respawn pane: {err}");
                    return;
                }
            }
            (None, None) => spawn_shell(&terminal, None, None),
        }
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.crashed = None;
        }
        self.rebuild_list();
    }

    /// Drop every group that has no tabs left. A pruned group's panes go with
    /// it — its `Browser` (kill Chromium, close the pane, remove the profile
    /// dir) and its `Android` (stop the session, close the pane) — so its
    /// pane host must be unparented first.
    fn prune_empty_groups(&mut self) {
        let live: HashSet<usize> = self.tabs.iter().map(|t| t.group).collect();
        if self.groups.iter().all(|g| live.contains(&g.id)) {
            return;
        }
        if let Some(attached) = self.attached_host
            && !live.contains(&attached)
        {
            self.browser_paned.set_end_child(gtk::Widget::NONE);
            self.attached_host = None;
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

    /// Make `id` the active tab. Returns whether anything changed.
    fn activate(&mut self, id: usize) -> bool {
        if self.active == Some(id) {
            return false;
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        self.active = Some(id);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == tab.group) {
            group.last_active = id;
        }
        self.stack.set_visible_child(&tab.view);
        tab.terminal.grab_focus();
        // A switch must never reorder the sidebar: the focus change can make
        // the program in the pane repaint (tmux focus-events, TUI redraw on
        // focus-in), and that repaint is the echo of the switch itself, not
        // new activity. Reopen the settle window so both stamp sources
        // (output, title) are deaf for a moment; genuine later output still
        // stamps and reorders.
        arm_later(&tab.armed, tab.armed.disarm());
        self.rebuild_list();
        // The main funnel for swapping the parented pane host. Not the
        // only one: a tab *move* changes the active group with no tab
        // change, so the move handlers re-sync themselves.
        self.sync_pane_host();
        true
    }

    fn close_tab(&mut self, id: usize) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let order = self.nav_order();
        let tab = self.tabs.remove(index);
        // Explicit close is the only thing that kills the backing session.
        match self.group_host(tab.group) {
            None => {
                if let Some(tmux) = &self.tmux
                    && let Err(err) = tmux.kill_session(&tab.uuid)
                {
                    eprintln!("failed to kill session {}: {err}", tab.uuid);
                }
            }
            // Remote: the queue is an outbox. The entry leaves it only when
            // the host confirms the kill (RemoteEvent::Killed), so a kill
            // lost to a dropped connection — or to quitting right after
            // closing the last tab — is retried after the next successful
            // connect instead of leaking the session. A live host gets the
            // kill at once, fire-and-forget.
            Some(host) => {
                self.pending_kills.push(state::PendingKill {
                    host: host.clone(),
                    uuid: tab.uuid.clone(),
                });
                if self.host_state(&host) == HostState::Live
                    && let Some(worker) = self.worker(&host)
                {
                    worker.kill(vec![tab.uuid.clone()]);
                }
            }
        }
        self.stack.remove(&tab.view);
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

    /// Ctrl-Page_Up/Down, walking all tabs in sidebar order as one list: a
    /// step inside the active group moves one row; a step past its last tab
    /// lands on the next group's first tab, a step back past its first tab
    /// on the previous group's last tab (`state::nav_target`). The order may
    /// reshuffle at any moment (an idle tab receives an update and, in
    /// Activity sort, jumps to the front), and recomputing the position on
    /// every press would make the walk jump with it. So a burst of
    /// navigation freezes the order it started with: the first press
    /// snapshots the groups and their member order, later presses keep
    /// stepping through that snapshot until `NAV_BURST_MS` passes without
    /// navigation — or the user lands elsewhere (a click, a jump, a close),
    /// which shows up as the active tab no longer being in the snapshot or a
    /// snapshotted tab being gone. The sidebar defers its own re-sort for the
    /// length of the burst, so the two stay in step.
    fn navigate(&mut self, step: isize) {
        let Some(active) = self.active else { return };
        let taken = self.nav_burst.borrow_mut().take();
        let order = match taken {
            Some((order, source)) => {
                source.remove();
                let intact = order
                    .iter()
                    .flat_map(|(_, tabs)| tabs)
                    .all(|id| self.tabs.iter().any(|t| t.id == *id));
                let anchored = order.iter().any(|(_, tabs)| tabs.contains(&active));
                if intact && anchored {
                    order
                } else {
                    self.snapshot_nav_order()
                }
            }
            None => self.snapshot_nav_order(),
        };
        let groups: Vec<state::NavGroup> = order
            .iter()
            .map(|(_, tabs)| state::NavGroup { tabs: tabs.clone() })
            .collect();
        // Reopen (and thereby extend) the burst window around the order just
        // used, so a reshuffle between presses cannot redirect the walk. When
        // it closes, the sidebar catches up on anything it held back. Stored
        // before the activation below so that its `rebuild_list` already
        // renders the frozen order (`group_members` reads the burst).
        let source = gtk::glib::timeout_add_local_once(Duration::from_millis(NAV_BURST_MS), {
            let nav_burst = self.nav_burst.clone();
            let input = self.input.clone();
            move || {
                *nav_burst.borrow_mut() = None;
                let _ = input.send(Msg::Resort);
            }
        });
        *self.nav_burst.borrow_mut() = Some((order, source));
        if let Some(target) = state::nav_target(&groups, active, step) {
            self.activate(target);
        }
    }

    /// The display order as a nav burst freezes it: every group in sidebar
    /// order with its member ids as `group_members` shows them.
    fn snapshot_nav_order(&self) -> Vec<(usize, Vec<usize>)> {
        self.groups
            .iter()
            .map(|g| {
                (
                    g.id,
                    self.group_members(g.id).into_iter().map(|t| t.id).collect(),
                )
            })
            .collect()
    }

    /// The tab a collapsed group row shows and keyboard navigation enters
    /// the group on: the group's last-active tab when it is still among
    /// `members` (display order), else the first member. `members` must not
    /// be empty.
    fn representative_of(&self, group: usize, members: &[usize]) -> usize {
        self.groups
            .iter()
            .find(|g| g.id == group)
            .map(|g| g.last_active)
            .filter(|id| members.contains(id))
            .unwrap_or(members[0])
    }

    /// Reorder by drag-and-drop: insert `src` before `dest`. If `dest` is in
    /// another group (e.g. a collapsed group's row), the tab moves there too —
    /// within-group order is just the tabs vec order, filtered per group.
    fn drop_tab(&mut self, src: usize, dest: usize) {
        if src == dest {
            return;
        }
        let group_of = |id: usize| self.tabs.iter().find(|t| t.id == id).map(|t| t.group);
        let (Some(src_group), Some(dest_group)) = (group_of(src), group_of(dest)) else {
            return;
        };
        if !state::drop_allowed(
            self.group_host(src_group).as_deref(),
            self.group_host(dest_group).as_deref(),
        ) {
            self.show_toast(CROSS_HOST_TOAST);
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
        self.sync_pane_host();
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
        let src_group = self.tabs[si].group;
        if !state::drop_allowed(
            self.group_host(src_group).as_deref(),
            self.group_host(group).as_deref(),
        ) {
            self.show_toast(CROSS_HOST_TOAST);
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
        self.sync_pane_host();
    }

    /// Jump to the representative tab of the next/previous group, wrapping
    /// around — the same tab its collapsed sidebar row shows.
    fn navigate_group(&mut self, step: isize) {
        let Some(current) = self.active_group() else {
            return;
        };
        let Some(pos) = self.groups.iter().position(|g| g.id == current) else {
            return;
        };
        let next = (pos as isize + step).rem_euclid(self.groups.len() as isize) as usize;
        let target = self.groups[next].id;
        let members: Vec<usize> = self.group_members(target).iter().map(|t| t.id).collect();
        if !members.is_empty() {
            self.activate(self.representative_of(target, &members));
        }
    }

    /// Re-render the sidebar from model state. The active tab's group is shown
    /// expanded (one row per tab); every other group collapses to a single row
    /// showing its last-active tab plus the group tab count.
    fn rebuild_list(&self) {
        while let Some(row) = self.tab_list.row_at_index(0) {
            self.tab_list.remove(&row);
        }
        {
            let mut keys = self.host_keys.borrow_mut();
            keys.clear();
            for host in self.groups.iter().filter_map(|g| g.host.as_ref()) {
                if !keys.contains(host) {
                    keys.push(host.clone());
                }
            }
        }

        let active_group = self.active_group();
        let mut shown = Vec::with_capacity(self.tabs.len());
        for group in &self.groups {
            let members = self.group_members(group.id);
            if members.is_empty() {
                continue;
            }
            shown.extend(members.iter().map(|t| t.id));
            self.tab_list.append(&self.make_group_header(group));
            if Some(group.id) == active_group {
                for tab in &members {
                    self.tab_list.append(&self.make_row(tab, group, None));
                }
            } else {
                let ids: Vec<usize> = members.iter().map(|t| t.id).collect();
                let representative = self.representative_of(group.id, &ids);
                let representative = members.iter().find(|t| t.id == representative).unwrap();
                self.tab_list
                    .append(&self.make_row(representative, group, Some(members.len())));
            }
        }

        *self.shown_order.borrow_mut() = shown;
        self.rebuild_tab_bar();

        // Re-select the active row with the row-selected handler muted, so
        // the programmatic selection is never echoed back as a Msg::Select.
        if let Some(active) = self.active {
            let mut i = 0;
            while let Some(row) = self.tab_list.row_at_index(i) {
                if row.widget_name() == active.to_string() {
                    self.reselecting.set(true);
                    self.tab_list.select_row(Some(&row));
                    self.reselecting.set(false);
                    self.scroll_row_into_view(&row);
                    break;
                }
                i += 1;
            }
        }
    }

    /// Bring the active sidebar row into the list scroller's visible range.
    ///
    /// `ListBox::select_row` neither scrolls nor focuses, and focus goes to
    /// the terminal on activation, so the viewport's scroll-to-focus never
    /// fires. Without this, the list only appears to follow the selection
    /// because collapsing/expanding groups changes the content height and the
    /// adjustment gets clamped as a side effect — which does nothing when the
    /// active group alone is taller than the viewport.
    ///
    /// Deferred to idle for the same reason as `scroll_active_into_view`:
    /// the rows were only just appended and have no allocation yet.
    fn scroll_row_into_view(&self, row: &gtk::ListBoxRow) {
        let scroller = self.list_scroller.clone();
        let list = self.tab_list.clone();
        let row = row.clone();
        gtk::glib::idle_add_local_once(move || {
            // rebuild_list runs on every title/age change; a later rebuild may
            // already have torn this row out, and a detached row's bounds are
            // garbage.
            if row.parent().as_ref() != Some(list.upcast_ref::<gtk::Widget>()) {
                return;
            }
            let Some(bounds) = row.compute_bounds(&list) else {
                return;
            };
            let (y, height) = (bounds.y() as f64, bounds.height() as f64);
            let vadj = scroller.vadjustment();
            let (value, page) = (vadj.value(), vadj.page_size());
            if y < value {
                vadj.set_value(y);
            } else if y + height > value + page {
                vadj.set_value(y + height - page);
            }
        });
    }

    /// Horizontal tab strip above the terminal, mirroring the active group.
    /// Only shown when that group has more than one tab; independent of the
    /// sidebar's visibility.
    fn rebuild_tab_bar(&self) {
        while let Some(child) = self.tab_bar.first_child() {
            self.tab_bar.remove(&child);
        }
        let members: Vec<&Tab> = match self.active_group() {
            Some(group) => self.group_members(group),
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
            let text = display_title(&tab.age_shown, &tab.title, tab.crashed, None);
            let label = gtk::Label::builder()
                .label(&text)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .width_chars(tab_label_min_chars(text.chars().count(), active))
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
            for class in tab_state_classes(tab.crashed, tab.claude.is_some()) {
                button.add_css_class(class);
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

    /// Recompute every tab's age prefix and repaint only the labels whose
    /// prefix actually changed. Two-phase for the borrow checker: the
    /// refreshers take `&self`.
    fn refresh_ages(&mut self) {
        let now = SystemTime::now();
        let changed: Vec<(usize, String)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                // Saturating: a backward clock jump shows "now" until real
                // time catches up.
                let elapsed = now
                    .duration_since(tab.last_activity.get())
                    .unwrap_or(Duration::ZERO);
                let prefix = age_prefix(elapsed);
                (prefix != tab.age_shown).then_some((tab.id, prefix))
            })
            .collect();
        for (id, prefix) in changed {
            let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
                continue;
            };
            self.tabs[index].age_shown = prefix;
            let tab = &self.tabs[index];
            self.refresh_sidebar_label(tab);
            self.refresh_tab_bar_label(tab);
        }
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
        if !title_changed(&tab.title, &title) {
            return;
        }
        // After the guard, never before it: VTE's `window-title-notify` fires
        // on every title *write*, including writes of an unchanged string, so
        // the signal is only a prompt to compare and never itself evidence of
        // change. The comparison also absorbs the attach-time title
        // re-emission for free, which is why v2 needs no settle window.
        // Both the stamp and the age relabel are activity side effects; a
        // title written inside an open settle window (attach or the echo of
        // an activation) is not evidence of activity, so neither fires until
        // the sources are armed again.
        if tab.armed.is_armed() {
            tab.last_activity.set(SystemTime::now());
            // `age_shown` caches what the labels render; the stamp above just
            // invalidated it, and the two refreshes below read it. Without this
            // the relabelled tab keeps showing its pre-stamp age ("3d · <new
            // title>") until the next tick — a value the code knows is wrong at
            // the moment it paints it. No disk write is involved.
            tab.age_shown = age_prefix(Duration::ZERO);
        }
        tab.title = title;
        let tab = self.tabs.iter().find(|t| t.id == id).unwrap();
        self.refresh_sidebar_label(tab);
        self.refresh_tab_bar_label(tab);
        self.resort_if_stale();
        // A real title change is one of the tagger's two triggers; the rate
        // limits inside keep animation-rate titles from running it.
        self.maybe_tag(id);
    }

    /// A group's tabs in display order. Manual keeps the vec order (which
    /// `save_state` persists and drag-and-drop edits); Activity sorts by the
    /// last activity stamp, most recent first, and leaves the vec alone —
    /// except during a keyboard-navigation burst, which pins the order the
    /// burst froze (tabs created since, unknown to the snapshot, lead).
    fn group_members(&self, group: usize) -> Vec<&Tab> {
        let mut members: Vec<&Tab> = self.tabs.iter().filter(|t| t.group == group).collect();
        match self.sidebar_order {
            SidebarOrder::Manual => members,
            SidebarOrder::Activity
                if let Some(frozen) = self
                    .nav_burst
                    .borrow()
                    .as_ref()
                    .and_then(|(order, _)| order.iter().find(|(g, _)| *g == group))
                    .map(|(_, tabs)| tabs.clone()) =>
            {
                members.sort_by_key(|t| frozen.iter().position(|id| *id == t.id));
                members
            }
            SidebarOrder::Activity => {
                // `activity_order` keeps two tabs that both read "now" in
                // vec order instead of swapping on every chunk of output.
                let now = unix_secs(SystemTime::now());
                let elapsed: Vec<u64> = members
                    .iter()
                    .map(|t| now.saturating_sub(unix_secs(t.last_activity.get())))
                    .collect();
                state::activity_order(&elapsed)
                    .into_iter()
                    .map(|i| members[i])
                    .collect()
            }
        }
    }

    /// Rebuild the list when an activity stamp has changed the display order
    /// since the last render. Cheap when nothing moved: one vec comparison.
    /// Held back while a keyboard-navigation burst is running, so the list
    /// cannot drift from the frozen order the keys walk; the burst's expiry
    /// sends a `Msg::Resort` that catches up.
    fn resort_if_stale(&self) {
        if self.sidebar_order != SidebarOrder::Activity || self.nav_burst.borrow().is_some() {
            return;
        }
        let stale = self.nav_order() != *self.shown_order.borrow();
        if stale {
            self.rebuild_list();
        }
    }

    fn set_sidebar_order(&mut self, order: SidebarOrder) {
        if self.sidebar_order == order {
            return;
        }
        self.sidebar_order = order;
        self.rebuild_list();
    }

    /// Update the sidebar row that displays `tab`, if any: its own row when
    /// its group is expanded, the group's representative row when collapsed
    /// (and only if `tab` is that representative — otherwise it isn't shown).
    fn refresh_sidebar_label(&self, tab: &Tab) {
        let (row_name, text) = if Some(tab.group) == self.active_group() {
            (
                tab.id.to_string(),
                display_title(&tab.age_shown, &tab.title, tab.crashed, None),
            )
        } else {
            let members: Vec<usize> = self.group_members(tab.group).iter().map(|t| t.id).collect();
            if self.representative_of(tab.group, &members) != tab.id {
                return;
            }
            (
                tab.id.to_string(),
                display_title(&tab.age_shown, &tab.title, tab.crashed, Some(members.len())),
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
            .group_members(tab.group)
            .iter()
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
        // Tooltip stays the raw title: it exists to show what the ellipsized
        // label cannot, and the prefix is already on the label.
        button.set_tooltip_text(Some(&tab.title));
        if let Some(label) = button.child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
            let active = self.active == Some(tab.id);
            let text = display_title(&tab.age_shown, &tab.title, tab.crashed, None);
            label.set_width_chars(tab_label_min_chars(text.chars().count(), active));
            label.set_label(&text);
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
        row_box.append(&gtk::Image::from_icon_name(self.drag_handle_icon));
        row_box.append(
            &gtk::Label::builder()
                .label(&title)
                .halign(gtk::Align::Start)
                .build(),
        );
        // Remote groups name their host (unless the name already is the
        // host) and show its state: nothing when live, a spinner while
        // connecting, a warning with the reason when disconnected.
        if let Some(host) = &group.host {
            row_box.append(&gtk::Image::from_icon_name("network-server-symbolic"));
            if group.name != *host {
                let label = gtk::Label::builder().label(host.as_str()).build();
                label.add_css_class("dim-label");
                row_box.append(&label);
            }
            match self.host_state(host) {
                HostState::Live => {}
                HostState::Connecting => {
                    row_box.append(&gtk::Spinner::builder().spinning(true).build());
                }
                HostState::Disconnected(err) => {
                    let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
                    warning.add_css_class("tmux-warning");
                    warning.set_tooltip_text(Some(&err.message(host)));
                    row_box.append(&warning);
                }
            }
        }

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

        let target = SidebarDropTarget::Header { group: group.id };
        let key = self.host_key(group.host.as_deref());
        header.add_controller(self.sidebar_drop_target(&header, target, key));

        header
    }

    /// The drag-payload key of a host in the current render.
    fn host_key(&self, host: Option<&str>) -> String {
        let Some(host) = host else {
            return "local".to_string();
        };
        self.host_keys
            .borrow()
            .iter()
            .position(|known| known == host)
            .map_or_else(|| "unknown".to_string(), |index| index.to_string())
    }

    /// A sidebar drop target for `row`, whose group runs on the host keyed
    /// `host_key`. The payload is preloaded, so hovering can already tell a
    /// cross-host tab: `enter`/`motion` answer no action (the compositor's
    /// no-drop cursor) and tint the row. The drop itself still goes through
    /// `dispatch_sidebar_drop` and the drop handlers, which stay the
    /// authority.
    fn sidebar_drop_target(
        &self,
        row: &gtk::ListBoxRow,
        target: SidebarDropTarget,
        host_key: String,
    ) -> gtk::DropTarget {
        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        drop.set_preload(true);
        let judge = Rc::new({
            let row = row.downgrade();
            move |drop: &gtk::DropTarget| -> gtk::gdk::DragAction {
                let refused = drop
                    .value()
                    .and_then(|value| value.get::<String>().ok())
                    .is_some_and(|payload| drop_refused(&payload, &host_key));
                if let Some(row) = row.upgrade() {
                    if refused {
                        row.add_css_class("drop-refused");
                    } else {
                        row.remove_css_class("drop-refused");
                    }
                }
                if refused {
                    gtk::gdk::DragAction::empty()
                } else {
                    gtk::gdk::DragAction::MOVE
                }
            }
        });
        drop.connect_enter({
            let judge = judge.clone();
            move |drop, _, _| judge(drop)
        });
        drop.connect_motion({
            let judge = judge.clone();
            move |drop, _, _| judge(drop)
        });
        drop.connect_leave({
            let row = row.downgrade();
            move |_| {
                if let Some(row) = row.upgrade() {
                    row.remove_css_class("drop-refused");
                }
            }
        });
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        drop
    }

    fn make_row(
        &self,
        tab: &Tab,
        group: &Group,
        collapsed_count: Option<usize>,
    ) -> gtk::ListBoxRow {
        let label = display_title(&tab.age_shown, &tab.title, tab.crashed, collapsed_count);
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
        // Rows of a disconnected host stay selectable (their page explains
        // and offers Reconnect) but read as unavailable.
        if let Some(host) = &group.host
            && matches!(self.host_state(host), HostState::Disconnected(_))
        {
            row.add_css_class("host-disconnected");
        }
        for class in tab_state_classes(tab.crashed, tab.claude.is_some()) {
            row.add_css_class(class);
        }

        // The host key rides along so every drop target can judge the drag
        // while it hovers, without looking the tab up.
        let key = self.host_key(group.host.as_deref());
        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        let src_id = tab.id;
        let payload_key = key.clone();
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("tab:{src_id}:{payload_key}").to_value(),
            ))
        });
        row.add_controller(drag);

        let target = SidebarDropTarget::Tab {
            tab: tab.id,
            group: group.id,
        };
        row.add_controller(self.sidebar_drop_target(&row, target, key));

        row
    }

    /// The group settings dialog. `Edit`: the active group's name and browser
    /// default URL, with its host shown read-only. `Create`: the same form
    /// headed by a Host entry, creating a remote group. Enter applies, Esc
    /// cancels, an empty name removes the header (or, in create mode,
    /// defaults to the host) and an empty URL clears the default.
    ///
    /// Host and URL are validated as they are typed and Apply is disabled
    /// while either is unusable: `adw::AlertDialog` closes on any response,
    /// so an error raised at submit time would have nowhere left to live.
    fn show_group_settings(&self, mode: SettingsMode) {
        let group = match mode {
            SettingsMode::Create => None,
            SettingsMode::Edit => {
                let Some(group) = self
                    .active_group()
                    .and_then(|id| self.groups.iter().find(|g| g.id == id))
                else {
                    return;
                };
                Some(group)
            }
        };
        let remote = group.is_none_or(|g| g.host.is_some());

        let rows = adw::PreferencesGroup::new();
        // Host first. Read-only on an existing group: its tabs live on that
        // host, and a group without tabs does not exist (it is pruned).
        let host_entry = match group {
            None => {
                let entry = adw::EntryRow::builder()
                    .title("Host")
                    .activates_default(true)
                    .build();
                rows.add(&entry);
                Some(entry)
            }
            Some(group) => {
                // A subtitle is Pango markup by default, and a host may
                // legally contain '&' or '<'.
                let subtitle = match group.host.as_deref() {
                    Some(host) => format!("{host}\nClose all tabs to change the host"),
                    None => "This computer".to_string(),
                };
                rows.add(
                    &adw::ActionRow::builder()
                        .title("Host")
                        .subtitle(subtitle)
                        .use_markup(false)
                        .build(),
                );
                None
            }
        };
        let name_row = adw::EntryRow::builder()
            .title("Name")
            .activates_default(true)
            .build();
        name_row.set_text(group.map_or("", |g| g.name.as_str()));
        let url_row = adw::EntryRow::builder()
            .title("Browser default URL")
            .activates_default(true)
            .build();
        url_row.set_text(
            group
                .and_then(|g| g.default_url.as_deref())
                .unwrap_or_default(),
        );
        rows.add(&name_row);
        rows.add(&url_row);

        // Stock Adwaita style class, so the CSS provider needs nothing added.
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .build();
        error.add_css_class("error");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&rows);
        if remote {
            let note = gtk::Label::builder()
                .label("The browser runs on this computer and isn't exposed to remote tabs.")
                .xalign(0.0)
                .wrap(true)
                .build();
            note.add_css_class("dim-label");
            content.append(&note);
        }
        content.append(&error);

        let (title, apply) = match mode {
            SettingsMode::Create => ("New Remote Group", "Create"),
            SettingsMode::Edit => ("Group Settings", "Apply"),
        };
        let dialog = adw::AlertDialog::new(Some(title), None);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("apply", apply);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");

        // Live validation: the only gate on Apply. Also runs once up front, so
        // a hand-edited `state.json` opens the dialog already showing why.
        // Everything inside the dialog is held weakly: this closure lives on
        // widgets *inside* it, so strong references would be a cycle.
        let validate = Rc::new({
            let dialog = dialog.downgrade();
            let error = error.clone();
            let url_row = url_row.downgrade();
            let host_entry = host_entry.as_ref().map(|entry| entry.downgrade());
            move || {
                let (Some(dialog), Some(url_row)) = (dialog.upgrade(), url_row.upgrade()) else {
                    return;
                };
                let host = host_entry
                    .as_ref()
                    .and_then(|weak| weak.upgrade())
                    .map(|entry| entry.text().to_string());
                // An empty host just keeps Create disabled: "enter a host"
                // shouted at a fresh, empty form would be noise.
                let host_blank = host.as_deref() == Some("");
                let problem = host
                    .as_deref()
                    .filter(|host| !host.is_empty())
                    .and_then(|host| state::validate_host(host).err())
                    .map(|err| err.to_string())
                    .or_else(|| {
                        browser::normalize_default_url(&url_row.text())
                            .err()
                            .map(|err| err.to_string())
                    });
                match problem {
                    Some(message) => {
                        error.set_text(&message);
                        error.set_visible(true);
                        dialog.set_response_enabled("apply", false);
                    }
                    None => {
                        error.set_visible(false);
                        dialog.set_response_enabled("apply", !host_blank);
                    }
                }
            }
        });
        validate();
        url_row.connect_changed({
            let validate = validate.clone();
            move |_| validate()
        });
        if let Some(entry) = &host_entry {
            entry.connect_changed({
                let validate = validate.clone();
                move |_| validate()
            });
        }

        let id = group.map(|g| g.id);
        let input = self.input.clone();
        dialog.connect_response(Some("apply"), move |_, _| {
            // Apply is only pressable while these parse, and what the URL
            // normalizes to — scheme lowercased, implied `http://` filled in —
            // is what gets stored. `None` covers both "cleared" and the
            // unreachable error.
            let default_url = browser::normalize_default_url(&url_row.text())
                .ok()
                .flatten();
            let name = name_row.text().to_string();
            let msg = match (&host_entry, id) {
                (Some(entry), _) => Msg::CreateRemoteGroup {
                    host: entry.text().to_string(),
                    name,
                    default_url,
                },
                (None, Some(id)) => Msg::ApplyGroupSettings {
                    id,
                    name,
                    default_url,
                },
                (None, None) => return,
            };
            let _ = input.send(msg);
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

/// Parse a namespaced sidebar DnD payload ("tab:<id>:<host-key>" / "group:<id>") and send
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
    let Some((kind, src, _)) = parse_drop_payload(&payload) else {
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

/// Split a sidebar payload into kind, id and (for tabs) host key. Pure.
fn parse_drop_payload(payload: &str) -> Option<(&str, usize, Option<&str>)> {
    let mut parts = payload.splitn(3, ':');
    let kind = parts.next()?;
    let id = parts.next()?.parse().ok()?;
    Some((kind, id, parts.next()))
}

/// Would dropping `payload` on a row whose group is keyed `target_key` move
/// a tab between hosts? Group payloads never are (reordering groups is always
/// allowed), and a payload without a key is left to the drop-time check.
/// Pure.
fn drop_refused(payload: &str, target_key: &str) -> bool {
    matches!(parse_drop_payload(payload), Some(("tab", _, Some(key))) if key != target_key)
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

/// The Android pair for a booted Android; `None` while it boots.
fn android_env(android: &Android) -> Option<[(&'static str, String); 2]> {
    let adb = android.adb()?;
    Some(crate::cli::android_env_pairs(
        &android.control_socket_path().to_string_lossy(),
        adb,
    ))
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
/// GDK reads `stride * height` bytes out of the buffer it is handed, so a
/// degenerate or short frame is a failure to report rather than something to
/// pass on to it. The other constraints are the ones
/// `gdk_memory_texture_new()` checks itself and answers with NULL — which the
/// binding's non-nullable return turns into a panic, so they have to be caught
/// here: a row must hold its pixels (4 bytes each in `R8g8b8a8`), and the
/// dimensions must survive the cast to the `i32` the call takes.
fn frame_backs_texture(width: u32, height: u32, stride: usize, len: usize) -> bool {
    const BYTES_PER_PIXEL: usize = 4;
    let row = (width as usize).saturating_mul(BYTES_PER_PIXEL);
    let needed = stride.saturating_mul(height as usize);
    width > 0
        && height > 0
        && width <= i32::MAX as u32
        && height <= i32::MAX as u32
        && stride >= row
        && needed > 0
        && len >= needed
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

/// Whether the keyboard focus is in `widget` or one of its descendants.
fn pane_has_focus(widget: &gtk::Widget) -> bool {
    widget.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN)
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

/// The directory a CLI-spawned tab starts in. Pure (`is_dir` is injected).
///
/// - A remote group's directory is a path on its host (or `None`, the remote
///   home): no local `is_dir()` check applies to it. But a `--create` request
///   carries the caller's *local* directory (the CLI only resolves it that way
///   for local groups), and it can still land in a remote group when the
///   live-model recheck finds one of that name. That path means nothing on
///   the host, so the tab starts in the remote home instead.
/// - Locally: without tmux, VTE's spawn just fails for a nonexistent
///   directory and the callback only logs to stderr, leaving a permanently
///   empty tab even though the CLI already printed a uuid and exited 0. Drop
///   the cwd so the tab starts in the default location instead (tmux itself
///   tolerates a missing -c directory, so this only matters for the no-tmux
///   path).
fn cli_spawn_cwd(
    remote: bool,
    via_create: bool,
    cwd: Option<PathBuf>,
    is_dir: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    match (remote, via_create) {
        (true, true) => None,
        (true, false) => cwd,
        (false, _) => cwd.filter(|dir| is_dir(dir)),
    }
}

/// May a remote tab whose client just exited (while its session lives on)
/// be reattached automatically? Not when the client barely ran: that is a
/// client that cannot attach, and reattaching would loop. Pure.
fn reattach_allowed(since_spawn: Option<Duration>) -> bool {
    since_spawn.is_none_or(|elapsed| elapsed >= REATTACH_GUARD)
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

/// Schedule one `Msg::Resort` for `ACTIVITY_RESORT_MS` from now unless one
/// is already pending. Called from the VTE stamp callbacks, which run outside
/// the message loop and, for `contents-changed`, per output chunk.
fn request_resort(pending: &Rc<Cell<bool>>, input: &relm4::Sender<Msg>) {
    if pending.replace(true) {
        return;
    }
    gtk::glib::timeout_add_local_once(Duration::from_millis(ACTIVITY_RESORT_MS), {
        let input = input.clone();
        move || {
            let _ = input.send(Msg::Resort);
        }
    });
}

/// The gate on a tab's output and title activity sources. A tab starts
/// deaf and waiting for its first output (the attach or spawn repaint);
/// that output opens a settle window, and only its expiry arms the gate.
/// Every activation reopens the window (`disarm`), since the pane's program
/// may repaint on focus — the echo of the switch itself, not activity. Each
/// window carries a generation, so a timer from a superseded window is a
/// no-op instead of arming early. Pure state: the timers live in
/// `arm_later`, which keeps this testable.
#[derive(Debug, Default)]
struct ActivityArm {
    armed: Cell<bool>,
    awaiting_output: Cell<bool>,
    generation: Cell<u32>,
}

impl ActivityArm {
    fn new() -> Self {
        Self {
            awaiting_output: Cell::new(true),
            ..Self::default()
        }
    }

    fn is_armed(&self) -> bool {
        self.armed.get()
    }

    /// Open a settle window: deaf from now until `arm` is called with the
    /// returned generation (and no first output is still outstanding).
    fn disarm(&self) -> u32 {
        self.armed.set(false);
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }

    /// A new client is being attached in the terminal (reattach after the
    /// client died, remote reconnect): deaf again until its attach repaint
    /// has arrived and settled, exactly like a fresh tab.
    fn respawn(&self) {
        self.awaiting_output.set(true);
        self.disarm();
    }

    /// Output arrived. For the first output after spawn this opens the
    /// settle window and returns its generation for the caller to `arm`
    /// later; otherwise `None`, and `is_armed` says whether it counts.
    fn output(&self) -> Option<u32> {
        if !self.awaiting_output.replace(false) {
            return None;
        }
        Some(self.disarm())
    }

    /// A settle window expired. Arms only if it is still the current window
    /// and the first output has been seen — an activation before the attach
    /// repaint must not arm ahead of it.
    fn arm(&self, generation: u32) {
        if generation == self.generation.get() && !self.awaiting_output.get() {
            self.armed.set(true);
        }
    }
}

/// Arm `armed` for `generation` once `ACTIVITY_SETTLE_MS` have passed.
fn arm_later(armed: &Rc<ActivityArm>, generation: u32) {
    gtk::glib::timeout_add_local_once(Duration::from_millis(ACTIVITY_SETTLE_MS), {
        let armed = armed.clone();
        move || armed.arm(generation)
    });
}

/// Whether a key press counts as tab activity. Strict by design: in an agent
/// tab Esc and Enter are the moments of intent (interrupt / submit), while
/// composing and scrolling are not — counting every keystroke would pin a tab
/// being read or edited in at "now". The controller sees GTK keyvals, so Esc
/// is cleanly distinguishable here from the ESC-prefixed escape sequences that
/// arrow keys put on the wire; a pty-byte tap could not make that distinction.
fn is_activity_key(key: gtk::gdk::Key) -> bool {
    matches!(
        key,
        gtk::gdk::Key::Escape
            | gtk::gdk::Key::Return
            | gtk::gdk::Key::KP_Enter
            | gtk::gdk::Key::ISO_Enter
    )
}

/// Whether an incoming terminal title is a real change against the one the tab
/// already shows. The dedupe anchor for the title activity source: agents churn
/// the title while they work, but a title rewritten to the same string says
/// nothing happened.
fn title_changed(current: &str, incoming: &str) -> bool {
    current != incoming
}

/// Whole seconds since the epoch. The activity sort compares stamps at this
/// granularity, on both sides of the subtraction: truncating only the
/// difference (`(now - stamp).as_secs()`) makes two tabs stamped a fraction
/// of a second apart tie or not depending on the sub-second phase of `now`,
/// so every re-sort could swap them back and forth.
fn unix_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decide a restored tab's `last_activity` from what the state file held.
/// `state.json` is meant to stay hand-readable, so the persisted seconds are
/// untrusted input: a value past what `SystemTime` can represent must fall
/// back to `now` rather than panic, since `UNIX_EPOCH + Duration` panics on
/// overflow and this would abort during restore, before any window is up.
/// Absent (an entry predating the field, or an adopted orphan with no
/// `SavedTab` at all) also seeds `now` — erring young once is better than
/// inventing a past.
fn seed_activity(persisted: Option<u64>, now: SystemTime) -> SystemTime {
    persisted
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
        .unwrap_or(now)
}

/// Age prefix for a tab: minute granularity, never seconds, uncapped at the
/// day end ("45d" is honest and needs no fourth unit).
fn age_prefix(elapsed: Duration) -> String {
    // Derived from the same bucket the activity sort uses, so equal labels
    // always mean equal sort keys.
    let secs = state::age_bucket(elapsed.as_secs());
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Text of the label shown for a tab, in the sidebar and in the tab bar: the
/// age prefix, then the crash-marked title when the label stands for one tab
/// of the expanded group, or "title (n)" when it stands in for a collapsed
/// group of n tabs. Shared by row construction, tab-bar construction and the
/// in-place refreshes so they can never drift apart.
/// The CSS classes that say what state a tab is in: `tab-crashed` for a
/// dead shell (red label), `tab-claude` for a tracked claude session (green
/// tint). Shared by the sidebar row and the tab-bar button.
fn tab_state_classes(crashed: Option<i32>, claude: bool) -> Vec<&'static str> {
    let mut classes = Vec::new();
    if crashed.is_some() {
        classes.push("tab-crashed");
    }
    if claude {
        classes.push("tab-claude");
    }
    classes
}

fn display_title(
    age: &str,
    title: &str,
    crashed: Option<i32>,
    collapsed_count: Option<usize>,
) -> String {
    match collapsed_count {
        Some(n) => format!("{age} · {title} ({n})"),
        None => format!("{age} · {}", crashed_tab_label(title, crashed)),
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

/// How a dialog response of the autostart or linger dialog is drawn.
fn response_appearance(appearance: autostart::Appearance) -> adw::ResponseAppearance {
    match appearance {
        autostart::Appearance::Default => adw::ResponseAppearance::Default,
        autostart::Appearance::Suggested => adw::ResponseAppearance::Suggested,
        autostart::Appearance::Destructive => adw::ResponseAppearance::Destructive,
    }
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
     \u{2022} Middle-click \u{2014} paste the selection\n\
     \u{2022} Ctrl+Shift+C / Ctrl+Shift+V \u{2014} copy the selection / paste the clipboard\n\
     \u{2022} Ctrl+click \u{2014} open a link\n\
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

/// Plain-text URLs the terminal underlines on hover and opens on Ctrl+click.
/// Trailing punctuation is left out so "see https://x.org/a." opens `/a`.
const URL_REGEX: &str = r#"\b(?:https?|ftp|file)://[^\s<>"'`()\[\]{}]*[^\s<>"'`()\[\]{}.,;:!?]"#;
/// PCRE2_MULTILINE, which VTE requires on every match regex.
const PCRE2_MULTILINE: u32 = 0x0000_0400;
/// PCRE2_CASELESS: `HTTPS://` is a URL too.
const PCRE2_CASELESS: u32 = 0x0000_0008;

/// Make links in the terminal recognisable: OSC 8 hyperlinks (tmux forwards
/// them, see `terminal-features` in `TMUX_CONF`) and bare URLs, both shown
/// with a pointer cursor on hover.
fn enable_links(terminal: &Terminal) {
    terminal.set_allow_hyperlink(true);
    match vte4::Regex::for_match(URL_REGEX, PCRE2_MULTILINE | PCRE2_CASELESS) {
        Ok(regex) => {
            let tag = terminal.match_add_regex(&regex, 0);
            terminal.match_set_cursor_name(tag, "pointer");
        }
        Err(err) => eprintln!("URL regex rejected: {err}"),
    }
}

/// Whether a primary click with these modifiers opens the link under it.
/// Ctrl, as in other VTE terminals: a plain click belongs to tmux (mouse is
/// on), which selects text and positions panes with it.
fn opens_link(state: gtk::gdk::ModifierType) -> bool {
    state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
}

/// Open the link under a Ctrl+click in the default handler.
///
/// Capture phase, so the click is seen before VTE forwards it to tmux as a
/// mouse event. The sequence is claimed only when there is a link to open;
/// every other click reaches VTE (and tmux) untouched.
fn attach_link_opener(terminal: &Terminal) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|gesture, _, x, y| {
        if !opens_link(gesture.current_event_state()) {
            return;
        }
        let Some(terminal) = gesture.widget().and_downcast::<Terminal>() else {
            return;
        };
        let Some(uri) = terminal
            .check_hyperlink_at(x, y)
            .or_else(|| terminal.check_match_at(x, y).0)
        else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let window = terminal.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(&uri).launch(
            window.as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                if let Err(err) = result {
                    eprintln!("failed to open {uri}: {err}");
                }
            },
        );
    });
    terminal.add_controller(click);
}

/// Whether a middle click with these modifiers is ours to paste. With Shift
/// held VTE bypasses mouse reporting and pastes PRIMARY by itself.
fn pastes_primary(state: gtk::gdk::ModifierType) -> bool {
    !state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
}

/// Paste the PRIMARY selection on a plain middle click, as X terminals do.
///
/// tmux has the mouse, and its root key table is wiped (`TMUX_CONF`), so a
/// plain middle click would otherwise do nothing. Capture phase and claimed,
/// so tmux never sees the click; programs in the pane that use the middle
/// button themselves lose it, as they do under tmux's stock paste binding.
fn attach_middle_paste(terminal: &Terminal) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_MIDDLE);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|gesture, _, _, _| {
        if !pastes_primary(gesture.current_event_state()) {
            return;
        }
        let Some(terminal) = gesture.widget().and_downcast::<Terminal>() else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        terminal.paste_primary();
    });
    terminal.add_controller(click);
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

/// Decode a raw waitpid-style status into a user-facing exit code.
/// Used in the no-tmux fallback, and by remote tabs to tell ssh's own 255 from the remote side's status.
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
    spawn_client(terminal, &ctl.spawn_argv(uuid, cwd, command, env));
}

/// Run a tmux client argv in the terminal: `tmux …` locally, or a remote
/// tab's `ssh … -- 'tmux …'`.
fn spawn_client(terminal: &Terminal, argv: &[String]) {
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

// ---------------------------------------------------------------------------
// Overview mode: the mode stack, each group's data and its monitors, the
// tagger, the data issues. Wiring only — the rules are in
// `overview::{model, layout, tagger, issues}` and tested there.
impl App {
    /// Whether the active group shows its overview; drives the header
    /// toggle. Remote groups never do.
    fn active_overview_mode(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.overview_mode && g.host.is_none())
    }

    /// Whether the active group runs on another host. Such groups have no
    /// overview in this version: the toggle is insensitive.
    fn active_group_is_remote(&self) -> bool {
        self.active_group()
            .and_then(|id| self.group_host(id))
            .is_some()
    }

    /// Take `id`'s group out of overview mode because one of its tabs was
    /// picked. Returns whether the mode changed.
    fn leave_overview_for(&mut self, id: usize) -> bool {
        let Some(group_id) = self.tabs.iter().find(|t| t.id == id).map(|t| t.group) else {
            return false;
        };
        match self.groups.iter_mut().find(|g| g.id == group_id) {
            Some(group) if group.overview_mode => {
                group.overview_mode = false;
                true
            }
            _ => false,
        }
    }

    /// Show the page the active group's mode asks for and, for the
    /// overview, feed the view. Reached from `sync_pane_host`, so every
    /// switch path restores the new group's mode. Coming back to the
    /// terminals hands focus to the active terminal, as `activate` does.
    fn sync_overview(&mut self) {
        let showing = self
            .mode_stack
            .visible_child_name()
            .is_some_and(|name| name.as_str() == MODE_OVERVIEW);
        let wanted = self.active_group().filter(|id| {
            self.groups
                .iter()
                .any(|g| g.id == *id && g.overview_mode && g.host.is_none())
        });
        let Some(group_id) = wanted else {
            if showing {
                self.mode_stack.set_visible_child_name(MODE_TERMINALS);
                if let Some(tab) = self.active_tab() {
                    tab.terminal.grab_focus();
                }
            }
            return;
        };
        // Loaded lazily: the first time a group with a root shows its map.
        let unloaded = self
            .groups
            .iter()
            .any(|g| g.id == group_id && g.overview.is_none() && g.overview_root.is_some());
        if unloaded {
            self.load_overview(group_id);
        }
        self.feed_overview(group_id);
        if !showing {
            self.mode_stack.set_visible_child_name(MODE_OVERVIEW);
        }
    }

    /// Hand the view a group's state: its name, the data (only when the
    /// group or its generation differs from the last feed, so the viewport
    /// survives a redraw), the tab chips, the issues and the selected tab —
    /// or the empty page when there is no root or it cannot be read.
    fn feed_overview(&mut self, group_id: usize) {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return;
        };
        let view = &self.overview_view;
        view.set_group_name(&overview_group_name(group));
        let (Some(root), Some(data)) = (&group.overview_root, &group.overview) else {
            view.set_empty(Some(EmptyState::NoRoot));
            self.fed_overview = None;
            return;
        };
        if let Some(error) = &data.error {
            view.set_empty(Some(EmptyState::Unreadable {
                root: root.clone(),
                error: error.clone(),
            }));
            self.fed_overview = None;
            return;
        }
        if self.fed_overview != Some((group_id, data.generation)) {
            view.set_data(data.graph.clone(), data.layout.clone());
            self.fed_overview = Some((group_id, data.generation));
        }
        view.set_issues(data.issues.clone());
        let (chips, unmapped) = self.overview_chips(group_id);
        view.set_tabs(chips, unmapped, self.tagging_available);
        view.set_selected_tab(self.active_tab().map(|t| t.uuid.clone()));
        view.set_empty(None);
    }

    /// Re-feed the tab chips alone (names, ages) while the overview is on
    /// screen; the age tick calls this.
    fn refresh_overview_tabs(&self) {
        let Some(group_id) = self.active_group() else {
            return;
        };
        let showing = self
            .mode_stack
            .visible_child_name()
            .is_some_and(|name| name.as_str() == MODE_OVERVIEW);
        if !showing {
            return;
        }
        let (chips, unmapped) = self.overview_chips(group_id);
        self.overview_view
            .set_tabs(chips, unmapped, self.tagging_available);
    }

    /// Build (or rebuild) a group's overview from its root: the graph, its
    /// layout, one monitor per directory, the issues. A root that is not a
    /// readable directory gives the error state (an empty graph, no
    /// monitors). No root, or a remote group, drops the data and with it the
    /// monitors.
    fn load_overview(&mut self, group_id: usize) {
        let root = self
            .groups
            .iter()
            .find(|g| g.id == group_id && g.host.is_none())
            .and_then(|g| g.overview_root.clone());
        let Some(root) = root else {
            if let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) {
                group.overview = None;
            }
            return;
        };
        let (error, sources) = match std::fs::metadata(&root) {
            Ok(meta) if meta.is_dir() => (None, scan_root(&root)),
            Ok(_) => (Some("not a directory".to_string()), Vec::new()),
            Err(err) => (Some(err.to_string()), Vec::new()),
        };
        let monitors = if error.is_none() {
            self.watch_overview_dirs(group_id, &root)
        } else {
            HashMap::new()
        };
        self.set_overview_graph(group_id, Graph::build(sources), error, Some(monitors));
    }

    /// Install `graph` as a group's overview: lay it out, bump the
    /// generation, derive the issues from it and the group's tags.
    /// `monitors` replaces the group's set when given and keeps it otherwise
    /// (a file change watches nothing new).
    fn set_overview_graph(
        &mut self,
        group_id: usize,
        graph: Graph,
        error: Option<String>,
        monitors: Option<HashMap<PathBuf, gio::FileMonitor>>,
    ) {
        let tabs = self.tagged_tabs(group_id);
        self.overview_gen += 1;
        let generation = self.overview_gen;
        let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) else {
            return;
        };
        let issues = issues::derive(&graph, &tabs);
        let placed = layout::layout(&graph, &Metrics::default());
        let monitors = match (monitors, group.overview.take()) {
            (Some(fresh), _) => fresh,
            (None, Some(old)) => old.monitors,
            (None, None) => HashMap::new(),
        };
        group.overview = Some(OverviewData {
            graph: Rc::new(graph),
            layout: Rc::new(placed),
            error,
            monitors,
            issues,
            generation,
        });
    }

    /// After a group's graph or tags changed: redraw when it is on screen,
    /// and republish the snapshot so `kabelsalat overview issues` sees the
    /// new issues. No state save: nothing here is the state file's.
    fn overview_changed(&mut self, group_id: usize) {
        if self.active_group() == Some(group_id) {
            self.sync_overview();
        }
        self.publish_groups();
    }

    /// A monitored `*.md` file changed or went away: rebuild the graph with
    /// that one source replaced or removed.
    fn overview_file_changed(&mut self, group_id: usize, path: &Path, removed: bool) {
        let Some(data) = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.overview.as_ref())
        else {
            return;
        };
        let graph = if removed || !path.is_file() {
            data.graph.without_source(path)
        } else {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string());
            data.graph.with_source((path.to_path_buf(), text))
        };
        self.set_overview_graph(group_id, graph, None, None);
        self.overview_changed(group_id);
    }

    /// A directory appeared below the root: watch it (and what is already
    /// below it) and take its files into the graph.
    fn overview_dir_created(&mut self, group_id: usize, dir: &Path) {
        if dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            return;
        }
        let Some(data) = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.overview.as_ref())
        else {
            return;
        };
        let mut sources: Vec<Source> = data
            .graph
            .sources()
            .iter()
            .filter(|(path, _)| !path.starts_with(dir))
            .cloned()
            .collect();
        sources.extend(scan_root(dir));
        let graph = Graph::build(sources);
        let fresh = self.watch_overview_dirs(group_id, dir);
        self.set_overview_graph(group_id, graph, None, None);
        if let Some(data) = self
            .groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .and_then(|g| g.overview.as_mut())
        {
            data.monitors.extend(fresh);
        }
        self.overview_changed(group_id);
    }

    /// One monitor for `root` and every directory below it (dot-dirs and
    /// symlinks skipped), each reporting its `*.md` changes and new
    /// directories into `update` for `group_id`.
    fn watch_overview_dirs(
        &self,
        group_id: usize,
        root: &Path,
    ) -> HashMap<PathBuf, gio::FileMonitor> {
        let mut monitors = HashMap::new();
        for dir in overview_dirs(root) {
            let file = gio::File::for_path(&dir);
            match file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            {
                Ok(monitor) => {
                    let input = self.input.clone();
                    monitor.connect_changed(move |_, file, other, event| {
                        let other = other.and_then(|f| f.path());
                        for msg in
                            overview_events(group_id, file.path(), other, event, Path::is_dir)
                        {
                            let _ = input.send(msg);
                        }
                    });
                    monitors.insert(dir, monitor);
                }
                Err(err) => eprintln!("kabelsalat: watching {}: {err}", dir.display()),
            }
        }
        monitors
    }

    /// The group's tagged tabs, as the issue derivation and the Resolve
    /// prompt want them.
    fn tagged_tabs(&self, group_id: usize) -> Vec<TaggedTab> {
        self.tabs
            .iter()
            .filter(|t| t.group == group_id)
            .filter_map(|t| {
                t.tags.as_ref().map(|tags| TaggedTab {
                    uuid: t.uuid.clone(),
                    name: t.title.clone(),
                    tags: tags.clone(),
                })
            })
            .collect()
    }

    /// The map's tab chips — one per link of every tagged tab — and the
    /// "Unmapped work" tray: tabs whose tagging found a topic but no node.
    fn overview_chips(&self, group_id: usize) -> (Vec<TabChip>, Vec<Unmapped>) {
        let now = SystemTime::now();
        let mut chips = Vec::new();
        let mut unmapped = Vec::new();
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            let Some(tags) = &tab.tags else { continue };
            let elapsed = now
                .duration_since(tab.last_activity.get())
                .unwrap_or(Duration::ZERO);
            let age = age_prefix(elapsed);
            for link in &tags.links {
                chips.push(TabChip {
                    uuid: tab.uuid.clone(),
                    name: tab.title.clone(),
                    role: link.role.clone(),
                    node: link.node.clone(),
                    activity: tags.activity.clone(),
                    age: age.clone(),
                });
            }
            if let Some(topic) = &tags.topic
                && tags.links.is_empty()
            {
                unmapped.push(Unmapped {
                    uuid: tab.uuid.clone(),
                    name: tab.title.clone(),
                    topic: topic.clone(),
                });
            }
        }
        (chips, unmapped)
    }

    /// Re-derive a group's issues after its tags changed (the graph did not).
    fn recompute_issues(&mut self, group_id: usize) {
        let tabs = self.tagged_tabs(group_id);
        if let Some(data) = self
            .groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .and_then(|g| g.overview.as_mut())
        {
            data.issues = issues::derive(&data.graph, &tabs);
        }
    }

    /// The `kabelsalat run`-shaped message that opens the Resolve tab for
    /// the active group's issue at `index`; `None` when the issue is gone.
    fn resolve_issue_command(&self, index: usize) -> Option<Msg> {
        let active = self.active_group()?;
        let group = self.groups.iter().find(|g| g.id == active)?;
        let root = group.overview_root.as_ref()?;
        let data = group.overview.as_ref()?;
        let issue = data.issues.get(index)?;
        let tabs = self.tagged_tabs(group.id);
        // A missing link names a tab: the first one linking both nodes.
        let tab = match (issue.kind, issue.nodes.as_slice()) {
            (issues::IssueKind::MissingLink, [a, b]) => tabs.iter().find(|t| {
                let links = |id: &str| t.tags.links.iter().any(|l| l.node == id);
                links(a) && links(b)
            }),
            _ => None,
        };
        let prompt = issues::resolve_prompt(issue, &data.graph, root, tab);
        Some(Msg::SpawnCommand {
            group: crate::cli::GroupTarget::Existing(group.uuid.clone()),
            tab_uuid: state::new_uuid(),
            cwd: Some(root.clone()),
            argv: vec!["claude".to_string(), prompt],
        })
    }

    /// The periodic trigger: offer every tab to `maybe_tag`; the
    /// one-at-a-time rule inside lets at most one run start per scan.
    fn overview_scan(&mut self) {
        let ids: Vec<usize> = self.tabs.iter().map(|t| t.id).collect();
        for id in ids {
            if self.tagger_running {
                break;
            }
            self.maybe_tag(id);
        }
    }

    /// Run the tagger for a tab when it is due: a local tab of a group with
    /// a non-empty overview, the command available, no run in flight, the
    /// per-tab gap elapsed, and enough new transcript (or a new title). The
    /// process runs off-thread and reports `Msg::TaggerDone`.
    fn maybe_tag(&mut self, tab_id: usize) {
        if !self.tagging_available || self.tagger_running {
            return;
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        if self.group_host(tab.group).is_some() {
            return; // remote groups are not tagged
        }
        let Some(graph) = self
            .groups
            .iter()
            .find(|g| g.id == tab.group)
            .and_then(|g| g.overview.as_ref())
            .map(|data| data.graph.clone())
        else {
            return;
        };
        if graph.is_empty() {
            return;
        }
        let last_run = tab.last_tag_run.map(|at| at.elapsed().as_secs());
        if !tagger::may_run(last_run, self.tagger_running) {
            return;
        }
        let Some((mark, text)) = tagger_input(tab) else {
            return;
        };
        let nodes: Vec<NodeSummary> = graph
            .nodes()
            .map(|n| NodeSummary {
                id: &n.id,
                kind: &n.kind,
                title: &n.title,
            })
            .collect();
        let current = tab.tags.clone().unwrap_or_default();
        let prompt = tagger::prompt(
            &self.tagger_config.roles,
            &nodes,
            &current,
            &tab.title,
            &text,
        );
        let tab_uuid = tab.uuid.clone();
        let argv = self.tagger_config.tagger_command.clone();
        let input = self.input.clone();
        self.tagger_running = true;
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.last_tag_run = Some(Instant::now());
        }
        let mark = mark.render();
        std::thread::spawn(move || {
            let result = run_tagger(&argv, &prompt);
            let _ = input.send(Msg::TaggerDone {
                tab_uuid,
                mark,
                result,
            });
        });
    }

    /// Apply a finished tagger run: the answer's tags replace the tab's and
    /// go to its tmux session with the watermark; a failed run or an
    /// unparsable answer keeps the old tags and mark (the next significant
    /// change retries).
    fn tagger_done(&mut self, tab_uuid: &str, mark: &str, result: Result<String, String>) {
        self.tagger_running = false;
        let Some((tab_id, group_id)) = self
            .tabs
            .iter()
            .find(|t| t.uuid == tab_uuid)
            .map(|t| (t.id, t.group))
        else {
            return; // the tab closed while the tagger ran
        };
        let stdout = match result {
            Ok(stdout) => stdout,
            Err(err) => {
                eprintln!("kabelsalat: tagger for tab {tab_uuid}: {err}");
                return;
            }
        };
        let tags = match tagger::parse_response(&stdout) {
            Ok(tags) => tags,
            Err(err) => {
                eprintln!("kabelsalat: tagger answer for tab {tab_uuid}: {err}");
                return;
            }
        };
        self.store_tags(tab_id, group_id, tags, mark);
        self.recompute_issues(group_id);
        self.overview_changed(group_id);
    }

    /// Keep a tab's tags in the model and — with tmux, for a local group —
    /// on its session, where they survive a GUI restart. Without tmux they
    /// live in memory only (spec section 8).
    fn store_tags(&mut self, tab_id: usize, group_id: usize, tags: Tags, mark: &str) {
        let local = self.group_host(group_id).is_none();
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
            return;
        };
        tab.tag_mark = Watermark::parse(mark);
        if let Some(tmux) = &self.tmux
            && local
        {
            match serde_json::to_string(&tags) {
                Ok(json) => {
                    if let Err(err) = tmux.set_environment(&tab.uuid, ENV_LINKS, &json) {
                        eprintln!("kabelsalat: set {ENV_LINKS} on {}: {err}", tab.uuid);
                    }
                }
                Err(err) => eprintln!("kabelsalat: serializing tags: {err}"),
            }
            if let Err(err) = tmux.set_environment(&tab.uuid, ENV_TAG_MARK, mark) {
                eprintln!("kabelsalat: set {ENV_TAG_MARK} on {}: {err}", tab.uuid);
            }
        }
        tab.tags = Some(tags);
    }

    /// Seed the tags of reattached local tabs from their tmux sessions,
    /// where the previous run left them. Anything unreadable starts untagged.
    fn restore_tags(&mut self, reattached: &[(String, usize)]) {
        let Some(tmux) = &self.tmux else { return };
        for (uuid, group) in reattached {
            if self.group_host(*group).is_some() {
                continue;
            }
            let tags = match tmux.show_environment(uuid, ENV_LINKS) {
                Ok(Some(json)) => serde_json::from_str::<Tags>(&json).ok(),
                _ => None,
            };
            let Some(tags) = tags else { continue };
            let mark = match tmux.show_environment(uuid, ENV_TAG_MARK) {
                Ok(Some(mark)) => Watermark::parse(&mark),
                _ => None,
            };
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.uuid == *uuid) {
                tab.tags = Some(tags);
                tab.tag_mark = mark;
            }
        }
    }
}

/// The name the overview shows for a group: the sidebar header's.
fn overview_group_name(group: &Group) -> String {
    if group.name.is_empty() {
        format!("Tab group {}", group.id)
    } else {
        group.name.clone()
    }
}

/// Whether `program` would be found by `Command::new`: as given when it
/// has a directory part, else in one of `path`'s directories.
fn on_path(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    path.is_some_and(|path| std::env::split_paths(path).any(|dir| dir.join(program).is_file()))
}

/// `root` and every directory below it, breadth first; directories whose
/// name starts with `.` and symlinks are skipped, like `scan_root` does.
fn overview_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let mut next = 0;
    while next < dirs.len() {
        if let Ok(entries) = std::fs::read_dir(&dirs[next]) {
            for entry in entries.flatten() {
                let hidden = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'));
                if !hidden && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    dirs.push(entry.path());
                }
            }
        }
        next += 1;
    }
    dirs
}

/// What a directory monitor event means for the overview: a `*.md` file
/// changed or went away, a directory appeared, or nothing of interest. On
/// a rename `file` is the old name and `other` the new one.
fn overview_events(
    group_id: usize,
    file: Option<PathBuf>,
    other: Option<PathBuf>,
    event: gio::FileMonitorEvent,
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<Msg> {
    let is_md = |path: &Path| path.extension().is_some_and(|ext| ext == "md");
    let changed = |path: PathBuf| Msg::OverviewFileChanged {
        group_id,
        path,
        removed: false,
    };
    let removed = |path: PathBuf| Msg::OverviewFileChanged {
        group_id,
        path,
        removed: true,
    };
    let Some(file) = file else {
        return Vec::new();
    };
    match event {
        gio::FileMonitorEvent::Created if is_dir(&file) => {
            vec![Msg::OverviewDirCreated {
                group_id,
                path: file,
            }]
        }
        gio::FileMonitorEvent::Created
        | gio::FileMonitorEvent::Changed
        | gio::FileMonitorEvent::ChangesDoneHint
        | gio::FileMonitorEvent::MovedIn => {
            if is_md(&file) {
                vec![changed(file)]
            } else {
                Vec::new()
            }
        }
        gio::FileMonitorEvent::Deleted | gio::FileMonitorEvent::MovedOut => {
            if is_md(&file) {
                vec![removed(file)]
            } else {
                Vec::new()
            }
        }
        gio::FileMonitorEvent::Renamed => {
            let mut out = Vec::new();
            if is_md(&file) {
                out.push(removed(file));
            }
            if let Some(new) = other.filter(|path| is_md(path)) {
                out.push(changed(new));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// What the tagger gets for a tab, and the watermark that run will leave:
/// the transcript since the current mark when the tab runs a claude whose
/// transcript grew significantly, else the title when it changed. `None`
/// when there is nothing new (keeping the old mark).
fn tagger_input(tab: &Tab) -> Option<(Watermark, String)> {
    if let Some(session) = &tab.claude
        && let Some(projects) = claude::projects_dir()
    {
        let path = claude::transcript_path(&projects, session);
        // A transcript that is not there yet falls through to the title; one
        // that cannot be read waits for the next scan.
        if let Ok(meta) = std::fs::metadata(&path) {
            let text = std::fs::read_to_string(&path).ok()?;
            let last = tagger::last_uuid(&text);
            if tagger::unchanged(tab.tag_mark.as_ref(), meta.len(), last.as_deref()) {
                return None;
            }
            let delta = tagger::delta_since(&text, tab.tag_mark.as_ref());
            if !tagger::significant(&delta) {
                return None;
            }
            let previous = match &tab.tag_mark {
                Some(Watermark::Transcript { uuid, .. }) => Some(uuid.clone()),
                _ => None,
            };
            let uuid = delta.last_uuid.clone().or(last).or(previous)?;
            return Some((
                Watermark::Transcript {
                    uuid,
                    offset: delta.end_offset,
                },
                delta.text,
            ));
        }
    }
    let (mark, text) = tagger::title_input(&tab.title);
    if tab.tag_mark.as_ref() == Some(&mark) {
        return None;
    }
    Some((mark, text))
}

/// How often the tagger thread checks whether its child exited.
const TAGGER_POLL: Duration = Duration::from_millis(200);

/// Run the tagger command with `prompt` on stdin and return its stdout.
/// Shaped like `android::run_capture`, but with a piped stdin and a reader
/// thread for stdout, so neither a long prompt nor a long answer can stall
/// the child on a full pipe. Kills the child after `tagger::TIMEOUT_SECS`.
fn run_tagger(argv: &[String], prompt: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let Some((program, args)) = argv.split_first() else {
        return Err("the tagger command is empty".to_string());
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{program}: {err}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the tagger has no stdout".to_string());
    };
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    if let Some(mut stdin) = child.stdin.take() {
        let written = stdin.write_all(prompt.as_bytes());
        // Dropped here: the child sees EOF.
        drop(stdin);
        if let Err(err) = written {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("writing the prompt: {err}"));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(tagger::TIMEOUT_SECS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TAGGER_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", tagger::TIMEOUT_SECS));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err.to_string());
            }
        }
    };
    let out = reader.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(format!("exited with {status}: {}", out.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- overview mode ------------------------------------------------------

    #[test]
    fn a_program_with_a_directory_part_is_looked_up_as_given() {
        assert!(on_path("/bin/sh", None));
        assert!(!on_path(
            "/nonexistent/kabelsalat-test/sh",
            Some(std::ffi::OsStr::new("/bin"))
        ));
    }

    #[test]
    fn a_bare_program_is_searched_on_path() {
        let path = std::ffi::OsStr::new("/nonexistent/kabelsalat-test:/bin");
        assert!(on_path("sh", Some(path)));
        assert!(!on_path("kabelsalat-no-such-program", Some(path)));
        assert!(!on_path("sh", None));
    }

    #[test]
    fn overview_dirs_walks_below_the_root_and_skips_dot_dirs() {
        let root = std::env::temp_dir().join(format!(
            "kabelsalat-app-test-overview-dirs-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join("a/n.md"), "x").unwrap();
        let mut dirs = overview_dirs(&root);
        dirs.sort();
        assert_eq!(dirs, vec![root.clone(), root.join("a"), root.join("a/b")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn monitor_events_name_md_files_and_new_directories() {
        let md = PathBuf::from("/r/n.md");
        let changed = overview_events(
            3,
            Some(md.clone()),
            None,
            gio::FileMonitorEvent::Changed,
            |_| false,
        );
        assert!(matches!(
            &changed[..],
            [Msg::OverviewFileChanged { group_id: 3, path, removed: false }] if *path == md
        ));
        let deleted = overview_events(
            3,
            Some(md.clone()),
            None,
            gio::FileMonitorEvent::Deleted,
            |_| false,
        );
        assert!(matches!(
            &deleted[..],
            [Msg::OverviewFileChanged { removed: true, .. }]
        ));
        let dir = PathBuf::from("/r/sub");
        let created = overview_events(
            3,
            Some(dir.clone()),
            None,
            gio::FileMonitorEvent::Created,
            |_| true,
        );
        assert!(matches!(
            &created[..],
            [Msg::OverviewDirCreated { group_id: 3, path }] if *path == dir
        ));
        let other = overview_events(
            3,
            Some(PathBuf::from("/r/x.txt")),
            None,
            gio::FileMonitorEvent::Changed,
            |_| false,
        );
        assert!(other.is_empty());
        let renamed = overview_events(
            3,
            Some(md),
            Some(PathBuf::from("/r/m.md")),
            gio::FileMonitorEvent::Renamed,
            |_| false,
        );
        assert!(matches!(
            &renamed[..],
            [
                Msg::OverviewFileChanged { removed: true, .. },
                Msg::OverviewFileChanged { removed: false, .. },
            ]
        ));
    }

    // --- sidebar selection echo -------------------------------------------

    #[test]
    fn programmatic_reselect_does_not_emit_select() {
        // The regression: rebuild_list's own select_row echoed back as a
        // Msg::Select, which could ping-pong forever once two tab switches
        // were queued back to back.
        assert_eq!(user_selection(true, Some(24)), None);
    }

    #[test]
    fn user_selection_of_a_row_emits_select() {
        assert_eq!(user_selection(false, Some(24)), Some(24));
    }

    #[test]
    fn deselection_never_emits_select() {
        assert_eq!(user_selection(false, None), None);
        assert_eq!(user_selection(true, None), None);
    }

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

    #[test]
    fn a_row_too_short_for_its_pixels_backs_nothing() {
        // Long enough overall, but GDK refuses a stride below width * 4 —
        // and answers a refusal with NULL, which would panic on the way back.
        assert!(!frame_backs_texture(800, 600, 1600, 1600 * 600));
        assert!(!frame_backs_texture(800, 600, 3199, 3199 * 600));
        // Exactly the pixels, no padding, is the normal case.
        assert!(frame_backs_texture(800, 600, 3200, 3200 * 600));
    }

    #[test]
    fn dimensions_beyond_i32_back_nothing() {
        // They would reach gdk_memory_texture_new() as negative numbers.
        let huge = i32::MAX as u32 + 1;
        assert!(!frame_backs_texture(
            huge,
            1,
            huge as usize * 4,
            huge as usize * 4
        ));
        assert!(!frame_backs_texture(1, huge, 4, huge as usize * 4));
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

    // --- tab state classes ------------------------------------------------

    #[test]
    fn tab_state_classes_mark_a_crash_and_a_tracked_claude() {
        // Shared by the sidebar row and the tab-bar button, so the two can
        // never disagree about what a tab looks like.
        assert!(tab_state_classes(None, false).is_empty());
        assert_eq!(tab_state_classes(Some(1), false), ["tab-crashed"]);
        assert_eq!(tab_state_classes(None, true), ["tab-claude"]);
        // A crashed claude tab is both: tinted, with the red label on top.
        assert_eq!(
            tab_state_classes(Some(-1), true),
            ["tab-crashed", "tab-claude"]
        );
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

    // --- tab age prefix ---------------------------------------------------

    #[test]
    fn age_under_a_minute_is_now() {
        assert_eq!(age_prefix(Duration::from_secs(0)), "now");
        assert_eq!(age_prefix(Duration::from_secs(59)), "now");
    }

    #[test]
    fn age_minutes() {
        assert_eq!(age_prefix(Duration::from_secs(60)), "1m");
        assert_eq!(age_prefix(Duration::from_secs(3_599)), "59m");
    }

    #[test]
    fn age_hours() {
        assert_eq!(age_prefix(Duration::from_secs(3_600)), "1h");
        assert_eq!(age_prefix(Duration::from_secs(86_399)), "23h");
    }

    #[test]
    fn age_days_are_uncapped() {
        assert_eq!(age_prefix(Duration::from_secs(86_400)), "1d");
        assert_eq!(age_prefix(Duration::from_secs(40 * 86_400)), "40d");
    }

    // --- sidebar/tab-bar label text ---------------------------------------

    #[test]
    fn expanded_row_shows_age_and_title_with_crash_marker() {
        assert_eq!(
            display_title("3d", "build", Some(1), None),
            "3d \u{b7} build [exit 1]"
        );
        assert_eq!(display_title("now", "bash", None, None), "now \u{b7} bash");
    }

    #[test]
    fn collapsed_row_shows_age_title_and_count() {
        // A collapsed group's row shows its member count, never the crash
        // marker (the representative stands in for the whole group) - but the
        // age prefix is always there.
        assert_eq!(
            display_title("2h", "web", None, Some(3)),
            "2h \u{b7} web (3)"
        );
        assert_eq!(
            display_title("5m", "bash", Some(3), Some(4)),
            "5m \u{b7} bash (4)"
        );
    }

    // --- tab activity sources (v2) ----------------------------------------

    #[test]
    fn activity_arm_opens_its_window_on_the_first_output() {
        let arm = ActivityArm::new();
        assert!(!arm.is_armed());
        // The attach repaint has not arrived: nothing may arm before it.
        let early = arm.disarm();
        arm.arm(early);
        assert!(!arm.is_armed());
        // First output opens the window; its expiry arms.
        let first = arm.output().expect("first output starts the settle window");
        assert!(!arm.is_armed());
        arm.arm(first);
        assert!(arm.is_armed());
        // Later output is plain activity, not another window.
        assert_eq!(arm.output(), None);
        assert!(arm.is_armed());
    }

    #[test]
    fn activity_arm_respawn_waits_for_the_new_attach_repaint() {
        let arm = ActivityArm::new();
        arm.arm(arm.output().unwrap());
        assert!(arm.is_armed());
        // Reconnect: the old window's timer and the new client's first
        // output must not arm early; only the new settle window does.
        arm.respawn();
        assert!(!arm.is_armed());
        let attach = arm.output().expect("attach repaint opens a window");
        assert!(!arm.is_armed());
        arm.arm(attach);
        assert!(arm.is_armed());
    }

    #[test]
    fn activity_sort_does_not_flap_on_sub_second_stamps() {
        // Two tabs stamped 0.2 s apart across a second boundary. Re-sorted
        // at any sub-second phase of `now`, they must keep one order.
        let a = UNIX_EPOCH + Duration::from_millis(1_000_900);
        let b = UNIX_EPOCH + Duration::from_millis(1_001_100);
        let mut orders = HashSet::new();
        for phase in (0..1000).step_by(50) {
            let now = UNIX_EPOCH + Duration::from_millis(1_000_000 + 600_000 + phase);
            let elapsed: Vec<u64> = [a, b]
                .iter()
                .map(|&t| unix_secs(now).saturating_sub(unix_secs(t)))
                .collect();
            orders.insert(state::activity_order(&elapsed));
        }
        assert_eq!(orders.len(), 1, "order flapped: {orders:?}");
    }

    #[test]
    fn middle_click_pastes_unless_shift_hands_it_to_vte() {
        use gtk::gdk::ModifierType;
        assert!(pastes_primary(ModifierType::empty()));
        assert!(pastes_primary(ModifierType::CONTROL_MASK));
        assert!(!pastes_primary(ModifierType::SHIFT_MASK));
    }

    #[test]
    fn links_open_on_ctrl_click_only() {
        use gtk::gdk::ModifierType;
        assert!(opens_link(ModifierType::CONTROL_MASK));
        assert!(opens_link(
            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
        ));
        assert!(!opens_link(ModifierType::empty()));
        assert!(!opens_link(ModifierType::SHIFT_MASK));
    }

    #[test]
    fn activity_arm_ignores_a_superseded_window() {
        let arm = ActivityArm::new();
        arm.arm(arm.output().unwrap());
        let older = arm.disarm();
        let newer = arm.disarm();
        arm.arm(older);
        assert!(!arm.is_armed(), "the older timer must not arm early");
        arm.arm(newer);
        assert!(arm.is_armed());
    }

    #[test]
    fn activity_arm_activation_before_attach_does_not_arm_ahead_of_it() {
        // Restore: add_tab, then activate (disarm), then the activation's
        // timer, then the attach repaint, then that window's timer.
        let arm = ActivityArm::new();
        let activation = arm.disarm();
        arm.arm(activation);
        assert!(!arm.is_armed());
        let attach = arm.output().unwrap();
        arm.arm(activation);
        assert!(!arm.is_armed());
        arm.arm(attach);
        assert!(arm.is_armed());
    }

    #[test]
    fn only_esc_and_enter_count_as_input_activity() {
        use gtk::gdk::Key;
        assert!(is_activity_key(Key::Escape));
        assert!(is_activity_key(Key::Return));
        assert!(is_activity_key(Key::KP_Enter));
        assert!(is_activity_key(Key::ISO_Enter));
    }

    #[test]
    fn composing_and_navigating_keys_stay_silent() {
        use gtk::gdk::Key;
        // Printable keys: typing into a prompt is not "the user did a thing".
        assert!(!is_activity_key(Key::a));
        assert!(!is_activity_key(Key::space));
        // Arrows send ESC-prefixed sequences on the wire but arrive here as
        // their own keyvals - which is the whole reason the filter sits on
        // keyvals and not on pty bytes.
        assert!(!is_activity_key(Key::Up));
        assert!(!is_activity_key(Key::Left));
        assert!(!is_activity_key(Key::Control_L));
        assert!(!is_activity_key(Key::Tab));
    }

    #[test]
    fn a_title_rewritten_to_the_same_string_is_not_activity() {
        // The case the whole dedupe exists for: VTE re-emits the title on
        // attach and on plain rewrites, so a write is not a change.
        assert!(!title_changed("claude", "claude"));
        assert!(title_changed("claude", "claude: building"));
        // The anchor is the *previous* title, not the set of titles ever
        // seen: A -> B -> A is two changes, not one.
        assert!(title_changed("B", "A"));
        // A restored tab anchors on the persisted `last_title`, so the
        // attach-time re-emission of that same title is silent.
        assert!(!title_changed("vim", "vim"));
    }

    // --- tab age seeding on restore ---------------------------------------

    #[test]
    fn persisted_activity_wins_over_now() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(
            seed_activity(Some(1_700_000_000), now),
            UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn absent_activity_seeds_now() {
        // An entry predating the field, or an adopted orphan with no
        // `SavedTab` at all: the tab reads "now", never an invented past.
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(seed_activity(None, now), now);
    }

    #[test]
    fn unrepresentable_activity_seeds_now_instead_of_panicking() {
        // `state.json` is hand-readable and therefore hand-editable, so the
        // persisted seconds are untrusted: `UNIX_EPOCH + Duration` would
        // panic on overflow and abort the app during restore, before any
        // window is up.
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(seed_activity(Some(u64::MAX), now), now);
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

    // --- remote reattach guard -------------------------------------------

    #[test]
    fn a_client_that_exits_right_after_attaching_is_not_reattached() {
        // Some clients exit immediately while their session lives on
        // ("open terminal failed"); reattaching those would loop.
        assert!(!reattach_allowed(Some(Duration::from_millis(300))));
        assert!(!reattach_allowed(Some(Duration::from_millis(1999))));
    }

    #[test]
    fn a_client_that_ran_for_a_while_is_reattached() {
        assert!(reattach_allowed(Some(Duration::from_secs(2))));
        assert!(reattach_allowed(Some(Duration::from_secs(3600))));
        // Never spawned: nothing to guard against.
        assert!(reattach_allowed(None));
    }

    #[test]
    fn only_ssh_s_own_status_counts_as_an_ssh_failure() {
        // Raw wait statuses: exit(255) is ssh failing; a signal or another
        // code is the remote side's business.
        assert_eq!(decode_exit(255 << 8), remote::SSH_FAILED);
        assert_ne!(decode_exit(1 << 8), remote::SSH_FAILED);
        assert_ne!(decode_exit(9), remote::SSH_FAILED);
    }

    // --- CLI spawn directory ---------------------------------------------

    #[test]
    fn a_remote_cli_tab_keeps_its_cwd_unchecked() {
        let never = |_: &Path| panic!("no local check for a remote path");
        assert_eq!(
            cli_spawn_cwd(true, false, Some(PathBuf::from("/srv/app")), never),
            Some(PathBuf::from("/srv/app"))
        );
        assert_eq!(cli_spawn_cwd(true, false, None, never), None);
    }

    #[test]
    fn a_create_request_landing_in_a_remote_group_drops_the_local_cwd() {
        let never = |_: &Path| panic!("no local check for a remote path");
        assert_eq!(
            cli_spawn_cwd(true, true, Some(PathBuf::from("/home/u/proj")), never),
            None
        );
    }

    #[test]
    fn a_local_cli_tab_keeps_only_an_existing_cwd() {
        let exists = |dir: &Path| dir == Path::new("/w");
        for via_create in [false, true] {
            assert_eq!(
                cli_spawn_cwd(false, via_create, Some(PathBuf::from("/w")), exists),
                Some(PathBuf::from("/w"))
            );
            assert_eq!(
                cli_spawn_cwd(false, via_create, Some(PathBuf::from("/gone")), exists),
                None
            );
        }
    }

    // --- cross-host drag and drop ----------------------------------------

    #[test]
    fn payloads_carry_kind_id_and_host_key() {
        assert_eq!(
            parse_drop_payload("tab:7:local"),
            Some(("tab", 7, Some("local")))
        );
        assert_eq!(parse_drop_payload("tab:7:2"), Some(("tab", 7, Some("2"))));
        assert_eq!(parse_drop_payload("group:3"), Some(("group", 3, None)));
        assert_eq!(parse_drop_payload("tab:x:local"), None);
        assert_eq!(parse_drop_payload("nonsense"), None);
    }

    #[test]
    fn a_tab_from_another_host_is_refused() {
        assert!(drop_refused("tab:7:0", "local"));
        assert!(drop_refused("tab:7:local", "1"));
        assert!(drop_refused("tab:7:0", "1"));
        assert!(!drop_refused("tab:7:1", "1"));
        assert!(!drop_refused("tab:7:local", "local"));
    }

    #[test]
    fn groups_may_always_be_reordered_and_unknown_payloads_are_left_to_the_drop() {
        assert!(!drop_refused("group:3", "1"));
        assert!(!drop_refused("tab:7", "1"));
        assert!(!drop_refused("garbage", "local"));
    }
}
