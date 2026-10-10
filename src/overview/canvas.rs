//! The overview widget: a zoomable canvas drawing the node graph at the four
//! tiers, the breadcrumb, zoom panel and trays floating over it, the
//! Close-tier card panel (rendered body, fields, links, the tabs on the node)
//! and the empty page. GTK wiring only: every rule it draws comes from the
//! pure modules (`model`, `layout`, `issues`), which carry the tests.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::f64::consts::{FRAC_PI_2, PI};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use relm4::adw;
use relm4::gtk::prelude::*;
use relm4::gtk::subclass::prelude::*;
use relm4::gtk::{self, cairo, gdk, glib, graphene, pango};

use super::issues::{self, Issue};
use super::layout::{self, BoxKey, Layout, MID_ROWS, PlacedBox, Rect, Tier, Viewport};
use super::model::{Color, FieldValue, Graph, Node};

// --- sizes (world units unless named *_PX) and colours ---

/// Text is never drawn smaller than this on screen, whatever the zoom.
const MIN_TEXT_PX: f64 = 10.0;
/// A row is never shorter than this on screen.
const MIN_ROW_PX: f64 = 16.0;
const BOX_RADIUS: f64 = 10.0;
const STRIPE_H: f64 = 6.0;
const PAD: f64 = 12.0;
const CARD_PAD: f64 = 10.0;
const ROW_H: f64 = 26.0;
const GRID_STEP: f64 = 40.0;
const GRID_MIN_PX: f64 = 20.0;
const FIT_MARGIN: f64 = 40.0;
const ZOOM_STEP: f64 = 1.25;
const WHEEL_ZOOM: f64 = 1.15;
const WHEEL_PAN_PX: f64 = 40.0;
const DRAG_THRESHOLD_PX: f64 = 3.0;
const PANEL_MIN_W: f64 = 480.0;
const PANEL_MIN_H: f64 = 320.0;
const PANEL_MARGIN: f64 = 12.0;
const ARROW_PX: f64 = 8.0;

const AMBER: gdk::RGBA = gdk::RGBA::new(0.90, 0.62, 0.11, 1.0);
const GREEN: gdk::RGBA = gdk::RGBA::new(0.18, 0.63, 0.36, 1.0);
const LINK_BLUE: gdk::RGBA = gdk::RGBA::new(0.21, 0.52, 0.89, 1.0);
const ACCENT: gdk::RGBA = gdk::RGBA::new(0.21, 0.52, 0.89, 1.0);
const MINT: gdk::RGBA = gdk::RGBA::new(0.45, 0.80, 0.62, 0.40);
const MUTED_GREY: gdk::RGBA = gdk::RGBA::new(0.5, 0.5, 0.5, 1.0);

const CSS: &str = "
.overview-float { background-color: alpha(@window_bg_color, 0.88); border-radius: 10px; padding: 6px 10px; }
.overview-tray { background-color: alpha(@window_bg_color, 0.88); border: 1px dashed alpha(currentColor, 0.45); border-radius: 10px; padding: 8px 10px; }
.overview-zoom { background-color: alpha(@window_bg_color, 0.88); border: 1px solid alpha(currentColor, 0.15); border-radius: 12px; padding: 8px; }
.overview-chip { background-color: alpha(currentColor, 0.08); border-radius: 999px; padding: 1px 9px; }
.overview-card-panel { background-color: @card_bg_color; color: @card_fg_color; border: 1px solid alpha(currentColor, 0.2); border-radius: 12px; }
.overview-card-panel textview, .overview-card-panel textview text { background-color: transparent; }
.overview-tab-row { padding: 4px 6px; border-radius: 8px; }
.overview-tab-row.selected { background-color: alpha(@accent_bg_color, 0.25); }
.overview-role-header { letter-spacing: 0.08em; }
";

// --- public API ---

/// What the app does when the user acts on the view.
pub struct Callbacks {
    /// A tab chip or tabs-panel row was clicked: the tab's uuid.
    pub open_tab: Rc<dyn Fn(String)>,
    /// A "Resolve" button was clicked: the index into the issues last given
    /// to [`OverviewView::set_issues`].
    pub resolve: Rc<dyn Fn(usize)>,
    /// A link in a rendered body that names no node.
    pub open_uri: Rc<dyn Fn(String)>,
}

/// One tab's link to a node, as drawn on the map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabChip {
    pub uuid: String,
    pub name: String,
    pub role: String,
    pub node: String,
    pub activity: Option<String>,
    pub age: String,
}

/// A tab whose tagging found no node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmapped {
    pub uuid: String,
    pub name: String,
    pub topic: String,
}

/// Why the canvas is replaced by the empty page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmptyState {
    NoRoot,
    Unreadable { root: PathBuf, error: String },
}

/// The overview page: cheap to clone, every clone shares the same widgets.
#[derive(Clone)]
pub struct OverviewView {
    inner: Rc<Inner>,
}

impl OverviewView {
    pub fn new(callbacks: Callbacks) -> Self {
        install_css();
        let inner = Inner::build(callbacks);
        Inner::wire(&inner);
        inner.update_empty_page();
        inner.viewport_changed();
        Self { inner }
    }

    /// The root widget to parent into the app (expands both ways).
    pub fn widget(&self) -> gtk::Widget {
        self.inner.root.clone().upcast()
    }

    /// The breadcrumb root ("<name> overview") and the empty-state command.
    pub fn set_group_name(&self, name: &str) {
        self.inner.state.borrow_mut().group_name = name.to_string();
        self.inner.update_empty_page();
        self.inner.viewport_changed();
    }

    /// Replace the model. The viewport is kept when the layout's bounds are
    /// unchanged (an edit to one file), else the whole map is fitted.
    pub fn set_data(&self, graph: Rc<Graph>, layout: Rc<Layout>) {
        let fit_now = {
            let mut s = self.inner.state.borrow_mut();
            let changed = s.layout.bounds != layout.bounds;
            s.graph = graph;
            s.layout = layout;
            s.bump();
            if changed {
                if s.viewport.w > 0.0 && s.viewport.h > 0.0 {
                    true
                } else {
                    s.needs_fit = true;
                    false
                }
            } else {
                false
            }
        };
        if fit_now {
            self.inner.fit_all();
        } else {
            self.inner.viewport_changed();
        }
    }

    pub fn set_tabs(&self, chips: Vec<TabChip>, unmapped: Vec<Unmapped>, tagging_available: bool) {
        {
            let mut s = self.inner.state.borrow_mut();
            let mut by_node: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, chip) in chips.iter().enumerate() {
                by_node.entry(chip.node.clone()).or_default().push(i);
            }
            s.chips = chips;
            s.tabs_by_node = by_node;
            s.unmapped = unmapped;
            s.tagging_available = tagging_available;
            s.bump();
        }
        self.inner.rebuild_unmapped();
        self.inner.viewport_changed();
    }

    /// The "Data issues" tray, the issue counts on boxes and the dashed
    /// warning edges (missing-link pairs).
    pub fn set_issues(&self, issues: Vec<Issue>) {
        {
            let mut s = self.inner.state.borrow_mut();
            s.warning_pairs = issues::missing_link_pairs(&issues);
            s.issues = issues;
            s.bump();
        }
        self.inner.rebuild_issues();
        self.inner.viewport_changed();
    }

    /// The tab selected in the sidebar: its chip and tabs-panel row are
    /// highlighted.
    pub fn set_selected_tab(&self, uuid: Option<String>) {
        {
            let mut s = self.inner.state.borrow_mut();
            s.selected = uuid;
            s.bump();
        }
        self.inner.viewport_changed();
    }

    /// `Some` replaces the canvas with the empty page; `None` shows the canvas.
    pub fn set_empty(&self, state: Option<EmptyState>) {
        let page = if state.is_some() { "empty" } else { "canvas" };
        self.inner.state.borrow_mut().empty = state;
        self.inner.stack.set_visible_child_name(page);
        self.inner.update_empty_page();
        self.inner.viewport_changed();
    }

    /// Centre the viewport on a node's primary copy at the Close scale.
    pub fn pan_to(&self, id: &str) {
        self.inner.pan_to(id);
    }
}

// --- shared state ---

/// A tab chip's screen rectangle, recorded while drawing and consulted by
/// the click handler.
struct Hit {
    rect: Rect,
    uuid: String,
}

/// What a box shows about its subtree.
#[derive(Clone)]
struct Summary {
    nodes: usize,
    tabs: usize,
    issues: usize,
    statuses: Vec<(Color, usize)>,
}

struct State {
    graph: Rc<Graph>,
    layout: Rc<Layout>,
    viewport: Viewport,
    /// Fit the bounds at the first allocation (data arrived before a size).
    needs_fit: bool,
    chips: Vec<TabChip>,
    tabs_by_node: HashMap<String, Vec<usize>>,
    unmapped: Vec<Unmapped>,
    issues: Vec<Issue>,
    warning_pairs: Vec<(String, String)>,
    selected: Option<String>,
    /// The hovered node when it has several copies (all are highlighted).
    hover: Option<String>,
    pointer: (f64, f64),
    dragging: bool,
    hits: Vec<Hit>,
    /// Where each visible box was drawn this frame, in screen coordinates
    /// (rows are drawn as list rows, not at their placed rectangles).
    drawn: HashMap<BoxKey, Rect>,
    focused: Option<BoxKey>,
    group_name: String,
    tagging_available: bool,
    empty: Option<EmptyState>,
    /// Bumped on every data change; the card panel is rebuilt when it moves.
    generation: u64,
    summaries: HashMap<String, Summary>,
}

impl State {
    fn new() -> State {
        State {
            graph: Rc::new(Graph::default()),
            layout: Rc::new(Layout::default()),
            viewport: Viewport::new(0.0, 0.0),
            needs_fit: true,
            chips: Vec::new(),
            tabs_by_node: HashMap::new(),
            unmapped: Vec::new(),
            issues: Vec::new(),
            warning_pairs: Vec::new(),
            selected: None,
            hover: None,
            pointer: (0.0, 0.0),
            dragging: false,
            hits: Vec::new(),
            drawn: HashMap::new(),
            focused: None,
            group_name: String::new(),
            tagging_available: true,
            empty: None,
            generation: 0,
            summaries: HashMap::new(),
        }
    }

    fn bump(&mut self) {
        self.generation += 1;
        self.summaries.clear();
    }

    /// Descendant count, distinct tabs, issue count and status shares of a
    /// node's subtree (the node itself when it has no descendants).
    fn summary(&mut self, id: &str) -> Summary {
        if let Some(s) = self.summaries.get(id) {
            return s.clone();
        }
        let desc = self.graph.descendants(id);
        let in_scope = |n: &str| n == id || desc.iter().any(|d| d == n);
        let summary = {
            let mut tabs: HashSet<&str> = HashSet::new();
            for chip in &self.chips {
                if in_scope(&chip.node) {
                    tabs.insert(&chip.uuid);
                }
            }
            let issues = self
                .issues
                .iter()
                .filter(|i| i.nodes.iter().any(|n| in_scope(n)))
                .count();
            let pool: Vec<&str> = if desc.is_empty() {
                vec![id]
            } else {
                desc.iter().map(String::as_str).collect()
            };
            let mut counts: HashMap<Color, usize> = HashMap::new();
            for n in pool {
                if let Some(node) = self.graph.node(n)
                    && let Some(c) = self.graph.status_color(node)
                {
                    *counts.entry(c).or_insert(0) += 1;
                }
            }
            let statuses: Vec<(Color, usize)> = Color::ALL
                .iter()
                .filter_map(|c| counts.get(c).map(|n| (*c, *n)))
                .collect();
            Summary {
                nodes: desc.len(),
                tabs: tabs.len(),
                issues,
                statuses,
            }
        };
        self.summaries.insert(id.to_string(), summary.clone());
        summary
    }
}

// --- the widget tree ---

struct Inner {
    me: Weak<Inner>,
    state: Rc<RefCell<State>>,
    callbacks: Callbacks,
    root: gtk::Box,
    stack: gtk::Stack,
    overlay: gtk::Overlay,
    canvas: OverviewCanvas,
    breadcrumb: gtk::Label,
    slider: gtk::Scale,
    zoom_out: gtk::Button,
    zoom_in: gtk::Button,
    zoom_fit: gtk::Button,
    issues_tray: gtk::Box,
    issues_rows: gtk::Box,
    unmapped_tray: gtk::Box,
    unmapped_rows: gtk::Box,
    panel: gtk::Box,
    panel_built: RefCell<Option<(BoxKey, u64)>>,
    empty_page: adw::StatusPage,
    pinch_last: Cell<f64>,
    drag_origin: Cell<(f64, f64)>,
}

impl Inner {
    fn build(callbacks: Callbacks) -> Rc<Inner> {
        let canvas = OverviewCanvas::new();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&canvas));

        let breadcrumb = gtk::Label::new(None);
        breadcrumb.set_halign(gtk::Align::Start);
        breadcrumb.set_valign(gtk::Align::Start);
        breadcrumb.set_margin_start(12);
        breadcrumb.set_margin_top(12);
        breadcrumb.set_xalign(0.0);
        breadcrumb.set_ellipsize(pango::EllipsizeMode::Middle);
        breadcrumb.set_max_width_chars(70);
        breadcrumb.add_css_class("overview-float");
        overlay.add_overlay(&breadcrumb);

        let slider = gtk::Scale::with_range(
            gtk::Orientation::Vertical,
            layout::MIN_SCALE.ln(),
            layout::MAX_SCALE.ln(),
            0.01,
        );
        slider.set_inverted(true);
        slider.set_draw_value(false);
        slider.set_size_request(-1, 170);
        for tier in Tier::ALL {
            slider.add_mark(
                tier.scale().ln(),
                gtk::PositionType::Left,
                Some(&format!("<small>{}</small>", tier.label())),
            );
        }
        let zoom_out = gtk::Button::with_label("\u{2212}");
        let zoom_in = gtk::Button::with_label("+");
        let zoom_fit = gtk::Button::with_label("Fit");
        let zoom_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
        zoom_panel.add_css_class("overview-zoom");
        zoom_panel.set_halign(gtk::Align::End);
        zoom_panel.set_valign(gtk::Align::End);
        zoom_panel.set_margin_end(12);
        zoom_panel.set_margin_bottom(12);
        zoom_panel.append(&slider);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        buttons.add_css_class("linked");
        buttons.set_halign(gtk::Align::Center);
        for b in [&zoom_out, &zoom_in, &zoom_fit] {
            b.add_css_class("flat");
            buttons.append(b);
        }
        zoom_panel.append(&buttons);
        overlay.add_overlay(&zoom_panel);

        let trays = gtk::Box::new(gtk::Orientation::Vertical, 8);
        trays.set_halign(gtk::Align::Start);
        trays.set_valign(gtk::Align::End);
        trays.set_margin_start(12);
        trays.set_margin_bottom(12);
        let (issues_tray, issues_rows) = build_tray("Data issues");
        let (unmapped_tray, unmapped_rows) = build_tray("Unmapped work");
        trays.append(&issues_tray);
        trays.append(&unmapped_tray);
        overlay.add_overlay(&trays);

        let panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        panel.add_css_class("overview-card-panel");
        panel.set_overflow(gtk::Overflow::Hidden);
        panel.set_visible(false);
        overlay.add_overlay(&panel);

        let empty_page = adw::StatusPage::new();
        empty_page.set_hexpand(true);
        empty_page.set_vexpand(true);

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.add_named(&overlay, Some("canvas"));
        stack.add_named(&empty_page, Some("empty"));
        stack.set_visible_child_name("canvas");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&stack);

        Rc::new_cyclic(|me| Inner {
            me: me.clone(),
            state: Rc::new(RefCell::new(State::new())),
            callbacks,
            root,
            stack,
            overlay,
            canvas,
            breadcrumb,
            slider,
            zoom_out,
            zoom_in,
            zoom_fit,
            issues_tray,
            issues_rows,
            unmapped_tray,
            unmapped_rows,
            panel,
            panel_built: RefCell::new(None),
            empty_page,
            pinch_last: Cell::new(1.0),
            drag_origin: Cell::new((0.0, 0.0)),
        })
    }

    /// Connect every signal. Closures hold a `Weak<Inner>`, never the `Rc`,
    /// because the widgets that own them are owned by `Inner`.
    fn wire(inner: &Rc<Inner>) {
        inner.canvas.imp().view.replace(inner.me.clone());

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let weak = inner.me.clone();
        scroll.connect_scroll(move |controller, dx, dy| {
            let Some(inner) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            inner.scrolled(controller, dx, dy);
            glib::Propagation::Stop
        });
        inner.canvas.add_controller(scroll);

        let motion = gtk::EventControllerMotion::new();
        let weak = inner.me.clone();
        motion.connect_motion(move |_, x, y| {
            if let Some(inner) = weak.upgrade() {
                inner.moved(x, y);
            }
        });
        let weak = inner.me.clone();
        motion.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                let changed = inner.state.borrow_mut().hover.take().is_some();
                if changed {
                    inner.canvas.queue_draw();
                }
            }
        });
        inner.canvas.add_controller(motion);

        let pinch = gtk::GestureZoom::new();
        let weak = inner.me.clone();
        pinch.connect_begin(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.pinch_last.set(1.0);
            }
        });
        let weak = inner.me.clone();
        pinch.connect_scale_changed(move |gesture, scale| {
            let Some(inner) = weak.upgrade() else { return };
            let last = inner.pinch_last.replace(scale);
            if last <= 0.0 || scale <= 0.0 {
                return;
            }
            let factor = scale / last;
            let (cx, cy) = gesture.bounding_box_center().unwrap_or_else(|| {
                let vp = inner.state.borrow().viewport;
                (vp.w / 2.0, vp.h / 2.0)
            });
            inner.update_viewport(|vp| vp.zoom_at(cx, cy, factor));
        });
        inner.canvas.add_controller(pinch);

        let drag = gtk::GestureDrag::new();
        let weak = inner.me.clone();
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                let mut s = inner.state.borrow_mut();
                s.dragging = false;
                inner.drag_origin.set((s.viewport.ox, s.viewport.oy));
            }
        });
        let weak = inner.me.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(inner) = weak.upgrade() else { return };
            if dx.abs() + dy.abs() < DRAG_THRESHOLD_PX {
                return;
            }
            inner.state.borrow_mut().dragging = true;
            let (ox, oy) = inner.drag_origin.get();
            inner.update_viewport(|vp| Viewport {
                ox: ox + dx,
                oy: oy + dy,
                ..vp
            });
        });
        inner.canvas.add_controller(drag);

        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let weak = inner.me.clone();
        click.connect_pressed(move |_, n_press, x, y| {
            if n_press == 2
                && let Some(inner) = weak.upgrade()
            {
                inner.double_clicked(x, y);
            }
        });
        let weak = inner.me.clone();
        click.connect_released(move |_, n_press, x, y| {
            if n_press == 1
                && let Some(inner) = weak.upgrade()
            {
                inner.clicked(x, y);
            }
        });
        inner.canvas.add_controller(click);

        // User input only (not the programmatic `set_value` from
        // `viewport_changed`), so no echo guard is needed.
        let weak = inner.me.clone();
        inner.slider.connect_change_value(move |_, _, value| {
            if let Some(inner) = weak.upgrade() {
                let scale = value.exp().clamp(layout::MIN_SCALE, layout::MAX_SCALE);
                inner.update_viewport(|vp| vp.set_scale_at_center(scale));
            }
            glib::Propagation::Proceed
        });
        let weak = inner.me.clone();
        inner.zoom_out.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.update_viewport(|vp| vp.set_scale_at_center(vp.scale / ZOOM_STEP));
            }
        });
        let weak = inner.me.clone();
        inner.zoom_in.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.update_viewport(|vp| vp.set_scale_at_center(vp.scale * ZOOM_STEP));
            }
        });
        let weak = inner.me.clone();
        inner.zoom_fit.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.fit_all();
            }
        });

        // The card panel sits over the focused card; every other overlay
        // keeps its halign/valign placement (`None`).
        let panel = inner.panel.clone();
        let state = inner.state.clone();
        inner
            .overlay
            .connect_get_child_position(move |overlay, widget| {
                if widget != panel.upcast_ref::<gtk::Widget>() {
                    return None;
                }
                let s = state.borrow();
                let key = s.focused.as_ref()?;
                let rect = s
                    .drawn
                    .get(key)
                    .copied()
                    .or_else(|| s.layout.get(key).map(|b| screen_rect(&s.viewport, &b.rect)))?;
                let cw = f64::from(overlay.width());
                let ch = f64::from(overlay.height());
                let w = rect
                    .w
                    .max(PANEL_MIN_W)
                    .min(cw - 2.0 * PANEL_MARGIN)
                    .max(1.0);
                let h = rect
                    .h
                    .max(PANEL_MIN_H)
                    .min(ch - 2.0 * PANEL_MARGIN)
                    .max(1.0);
                let (cx, cy) = rect.center();
                let x =
                    (cx - w / 2.0).clamp(PANEL_MARGIN, (cw - w - PANEL_MARGIN).max(PANEL_MARGIN));
                let y =
                    (cy - h / 2.0).clamp(PANEL_MARGIN, (ch - h - PANEL_MARGIN).max(PANEL_MARGIN));
                Some(gdk::Rectangle::new(x as i32, y as i32, w as i32, h as i32))
            });
    }

    // --- viewport ---

    fn update_viewport(&self, f: impl FnOnce(Viewport) -> Viewport) {
        {
            let mut s = self.state.borrow_mut();
            let next = f(s.viewport);
            s.viewport = next;
        }
        self.viewport_changed();
    }

    fn fit_all(&self) {
        let bounds = self.state.borrow().layout.bounds;
        self.update_viewport(|vp| vp.fit(&bounds, FIT_MARGIN));
    }

    fn pan_to(&self, id: &str) {
        let target = {
            let s = self.state.borrow();
            let copies = s.layout.copies(id);
            copies
                .iter()
                .find(|b| !b.mirror)
                .or_else(|| copies.first())
                .map(|b| b.rect)
        };
        let Some(rect) = target else { return };
        self.update_viewport(|vp| {
            let scale = Tier::Close.scale();
            let (cx, cy) = rect.center();
            Viewport {
                scale,
                ox: vp.w / 2.0 - cx * scale,
                oy: vp.h / 2.0 - cy * scale,
                ..vp
            }
        });
    }

    /// After any viewport or data change: slider, breadcrumb, card panel and
    /// a redraw.
    fn viewport_changed(&self) {
        let (scale, group, crumbs, focused, generation, empty) = {
            let mut s = self.state.borrow_mut();
            let vp = s.viewport;
            let tier = vp.tier();
            let (cx, cy) = vp.center_world();
            let ids = layout::breadcrumb(&s.layout, &s.graph, tier, cx, cy);
            let crumbs: Vec<String> = ids
                .iter()
                .map(|id| {
                    s.graph
                        .node(id)
                        .map_or_else(|| id.clone(), |n| n.title.clone())
                })
                .collect();
            let focused = if tier == Tier::Close {
                layout::focused(&s.layout, tier, cx, cy).map(|b| b.key.clone())
            } else {
                None
            };
            s.focused = focused.clone();
            (
                vp.scale,
                s.group_name.clone(),
                crumbs,
                focused,
                s.generation,
                s.empty.is_some(),
            )
        };
        self.slider.set_value(scale.ln());
        self.breadcrumb
            .set_markup(&breadcrumb_markup(&group, &crumbs));
        self.sync_panel(focused.filter(|_| !empty), generation);
        self.canvas.queue_draw();
    }

    // --- input ---

    fn scrolled(&self, controller: &gtk::EventControllerScroll, dx: f64, dy: f64) {
        let mods = controller.current_event_state();
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
        let wheel = controller.unit() == gdk::ScrollUnit::Wheel;
        let (px, py) = self.state.borrow().pointer;
        if ctrl || (wheel && !shift) {
            // Continuous zoom at the pointer: the mouse wheel in steps, a
            // touchpad with Ctrl held by its pixel distance.
            let factor = if wheel {
                WHEEL_ZOOM.powf(-dy)
            } else {
                (-dy / 120.0).exp()
            };
            self.update_viewport(|vp| vp.zoom_at(px, py, factor));
        } else if wheel {
            // Shift + wheel pans (GDK may already have turned it horizontal).
            self.update_viewport(|vp| vp.pan(-dx * WHEEL_PAN_PX, -dy * WHEEL_PAN_PX));
        } else {
            // Two-finger touchpad scroll pans by its pixel distance.
            self.update_viewport(|vp| vp.pan(-dx, -dy));
        }
    }

    fn moved(&self, x: f64, y: f64) {
        let (changed, over_chip) = {
            let mut s = self.state.borrow_mut();
            s.pointer = (x, y);
            let over_chip = s.hits.iter().any(|h| h.rect.contains(x, y));
            let (wx, wy) = s.viewport.to_world(x, y);
            let tier = s.viewport.tier();
            let hover = layout::hit_test(&s.layout, tier, wx, wy)
                .filter(|b| s.layout.copies(&b.key.id).len() > 1)
                .map(|b| b.key.id.clone());
            let changed = hover != s.hover;
            s.hover = hover;
            (changed, over_chip)
        };
        self.canvas
            .set_cursor_from_name(if over_chip { Some("pointer") } else { None });
        if changed {
            self.canvas.queue_draw();
        }
    }

    fn clicked(&self, x: f64, y: f64) {
        let uuid = {
            let s = self.state.borrow();
            if s.dragging {
                None
            } else {
                s.hits
                    .iter()
                    .rev()
                    .find(|h| h.rect.contains(x, y))
                    .map(|h| h.uuid.clone())
            }
        };
        if let Some(uuid) = uuid {
            (self.callbacks.open_tab)(uuid);
        }
    }

    fn double_clicked(&self, x: f64, y: f64) {
        let target = {
            let s = self.state.borrow();
            let (wx, wy) = s.viewport.to_world(x, y);
            layout::hit_test(&s.layout, s.viewport.tier(), wx, wy).map(|b| b.rect)
        };
        if let Some(rect) = target {
            self.update_viewport(|vp| vp.fit(&rect, FIT_MARGIN));
        }
    }

    /// A link in the rendered body: a node id or node file pans there,
    /// anything else goes to the app.
    fn follow_link(&self, href: &str) {
        let node = self
            .state
            .borrow()
            .graph
            .node_for_href(href)
            .map(|n| n.id.clone());
        match node {
            Some(id) => self.pan_to(&id),
            None => (self.callbacks.open_uri)(href.to_string()),
        }
    }

    // --- overlays ---

    fn update_empty_page(&self) {
        let (empty, group) = {
            let s = self.state.borrow();
            (s.empty.clone(), s.group_name.clone())
        };
        match empty {
            None => {}
            Some(EmptyState::NoRoot) => {
                self.empty_page.set_icon_name(Some("folder-symbolic"));
                self.empty_page.set_title("No overview root");
                let group = if group.is_empty() {
                    "<group>".to_string()
                } else {
                    shell_word(&group)
                };
                self.empty_page.set_description(Some(&format!(
                    "Set one with  <tt>kabelsalat overview root -g {} DIR</tt>  \u{2014} the node \
                     format is documented in the kabelsalat skill's overview-format.md",
                    glib::markup_escape_text(&group)
                )));
            }
            Some(EmptyState::Unreadable { root, error }) => {
                self.empty_page
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.empty_page.set_title("Overview root unreadable");
                self.empty_page.set_description(Some(&format!(
                    "<tt>{}</tt>\n{}",
                    glib::markup_escape_text(&root.display().to_string()),
                    glib::markup_escape_text(&error)
                )));
            }
        }
    }

    fn rebuild_unmapped(&self) {
        clear_children(&self.unmapped_rows);
        let unmapped = self.state.borrow().unmapped.clone();
        for tab in &unmapped {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let name = gtk::Label::new(None);
            name.set_markup(&format!(
                "<span foreground=\"{}\">\u{25cf}</span> {}",
                hex(&GREEN),
                glib::markup_escape_text(&tab.name)
            ));
            name.set_xalign(0.0);
            name.set_ellipsize(pango::EllipsizeMode::End);
            name.set_max_width_chars(36);
            row.append(&name);
            if !tab.topic.is_empty() {
                let topic = gtk::Label::new(Some(&tab.topic));
                topic.set_xalign(0.0);
                topic.set_ellipsize(pango::EllipsizeMode::End);
                topic.set_max_width_chars(40);
                topic.add_css_class("dim-label");
                topic.add_css_class("caption");
                row.append(&topic);
            }
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.set_child(Some(&row));
            let open_tab = self.callbacks.open_tab.clone();
            let uuid = tab.uuid.clone();
            button.connect_clicked(move |_| open_tab(uuid.clone()));
            self.unmapped_rows.append(&button);
        }
        self.unmapped_tray.set_visible(!unmapped.is_empty());
    }

    fn rebuild_issues(&self) {
        clear_children(&self.issues_rows);
        let issues = self.state.borrow().issues.clone();
        for (index, issue) in issues.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            let kind = gtk::Label::new(None);
            kind.set_markup(&format!(
                "<b>{}</b>  {}",
                glib::markup_escape_text(issue.kind.label()),
                glib::markup_escape_text(&issue.nodes.join(" \u{00b7} "))
            ));
            kind.set_xalign(0.0);
            kind.set_ellipsize(pango::EllipsizeMode::End);
            kind.set_max_width_chars(48);
            text.append(&kind);
            let detail = gtk::Label::new(Some(&issue.detail));
            detail.set_xalign(0.0);
            detail.set_wrap(true);
            detail.set_wrap_mode(pango::WrapMode::WordChar);
            detail.set_max_width_chars(48);
            detail.add_css_class("dim-label");
            detail.add_css_class("caption");
            text.append(&detail);
            row.append(&text);
            let resolve = gtk::Button::with_label("Resolve");
            resolve.add_css_class("pill");
            resolve.set_valign(gtk::Align::Center);
            let callback = self.callbacks.resolve.clone();
            resolve.connect_clicked(move |_| callback(index));
            row.append(&resolve);
            self.issues_rows.append(&row);
        }
        self.issues_tray.set_visible(!issues.is_empty());
    }

    /// Show the card panel over `focused` (rebuilding its children only when
    /// the focused key or the data changed), or hide it.
    fn sync_panel(&self, focused: Option<BoxKey>, generation: u64) {
        match focused {
            None => self.panel.set_visible(false),
            Some(key) => {
                let built = self.panel_built.borrow().clone();
                let current = (key, generation);
                if built.as_ref() != Some(&current) {
                    self.rebuild_panel(&current.0);
                    self.panel_built.replace(Some(current));
                }
                self.panel.set_visible(true);
            }
        }
        self.overlay.queue_allocate();
    }

    fn rebuild_panel(&self, key: &BoxKey) {
        clear_children(&self.panel);
        let data = {
            let s = self.state.borrow();
            let Some(node) = s.graph.node(&key.id) else {
                return;
            };
            let title_of = |id: &String| {
                s.graph
                    .node(id)
                    .map_or_else(|| id.clone(), |n| n.title.clone())
            };
            let chips: Vec<TabChip> = s
                .tabs_by_node
                .get(&key.id)
                .map(|idx| {
                    idx.iter()
                        .filter_map(|i| s.chips.get(*i).cloned())
                        .collect()
                })
                .unwrap_or_default();
            PanelData {
                node: node.clone(),
                status_color: s.graph.status_color(node),
                parents: s.graph.parents(&key.id).iter().map(&title_of).collect(),
                links: node
                    .links
                    .iter()
                    .map(|l| (l.name.clone(), title_of(&l.target)))
                    .collect(),
                chips,
                selected: s.selected.clone(),
                tagging_available: s.tagging_available,
            }
        };

        // Left column: title, chips, the body, the links footer.
        let left = gtk::Box::new(gtk::Orientation::Vertical, 8);
        left.set_hexpand(true);
        left.set_vexpand(true);
        left.set_margin_start(16);
        left.set_margin_end(16);
        left.set_margin_top(14);
        left.set_margin_bottom(14);

        let title = gtk::Label::new(Some(&data.node.title));
        title.add_css_class("title-2");
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_wrap_mode(pango::WrapMode::WordChar);
        left.append(&title);

        let chips = gtk::FlowBox::new();
        chips.set_selection_mode(gtk::SelectionMode::None);
        chips.set_homogeneous(false);
        chips.set_min_children_per_line(1);
        chips.set_max_children_per_line(12);
        chips.set_column_spacing(6);
        chips.set_row_spacing(6);
        chips.set_can_focus(false);
        if let Some(status) = &data.node.status {
            chips.insert(&chip_label(status, data.status_color), -1);
        }
        for parent in &data.parents {
            chips.insert(&chip_label(&format!("in: {parent}"), None), -1);
        }
        for (name, value) in &data.node.fields {
            chips.insert(
                &chip_label(&format!("{name}: {}", field_text(value)), None),
                -1,
            );
        }
        left.append(&chips);
        left.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(2);
        view.set_right_margin(2);
        view.set_top_margin(4);
        view.set_bottom_margin(4);
        let mut links: HashMap<String, String> = HashMap::new();
        render_markdown(&data.node.body, &view.buffer(), &mut links);
        let links = Rc::new(links);
        let link_click = gtk::GestureClick::new();
        link_click.set_button(gdk::BUTTON_PRIMARY);
        let weak = self.me.clone();
        link_click.connect_released(move |gesture, _, x, y| {
            let Some(view) = gesture
                .widget()
                .and_then(|w| w.downcast::<gtk::TextView>().ok())
            else {
                return;
            };
            let (bx, by) =
                view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let Some(iter) = view.iter_at_location(bx, by) else {
                return;
            };
            let href = iter
                .tags()
                .iter()
                .find_map(|tag| tag.name().and_then(|n| links.get(n.as_str()).cloned()));
            if let Some(href) = href
                && let Some(inner) = weak.upgrade()
            {
                inner.follow_link(&href);
            }
        });
        view.add_controller(link_click);
        let body = gtk::ScrolledWindow::new();
        body.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        body.set_hexpand(true);
        body.set_vexpand(true);
        body.set_child(Some(&view));
        left.append(&body);

        if !data.links.is_empty() {
            let text = data
                .links
                .iter()
                .map(|(name, target)| {
                    format!(
                        "{} \u{2192} <span foreground=\"{}\">{}</span>",
                        glib::markup_escape_text(name),
                        hex(&LINK_BLUE),
                        glib::markup_escape_text(target)
                    )
                })
                .collect::<Vec<_>>()
                .join(" \u{00b7} ");
            let footer = gtk::Label::new(None);
            footer.set_markup(&format!("Links: {text}"));
            footer.set_xalign(0.0);
            footer.set_wrap(true);
            footer.set_wrap_mode(pango::WrapMode::WordChar);
            footer.add_css_class("dim-label");
            footer.add_css_class("caption");
            left.append(&footer);
        }
        self.panel.append(&left);
        self.panel
            .append(&gtk::Separator::new(gtk::Orientation::Vertical));

        // Right column: the tabs on this node, grouped by role.
        let right = gtk::Box::new(gtk::Orientation::Vertical, 4);
        right.set_width_request(220);
        right.set_margin_start(12);
        right.set_margin_end(12);
        right.set_margin_top(14);
        right.set_margin_bottom(14);
        let heading = gtk::Label::new(Some(&format!(
            "Tabs on this node \u{00b7} {}",
            data.chips.len()
        )));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        right.append(&heading);
        if data.chips.is_empty() {
            let note = gtk::Label::new(Some(if data.tagging_available {
                "No tabs on this node"
            } else {
                "Tagging unavailable (no tagger command)"
            }));
            note.set_xalign(0.0);
            note.set_wrap(true);
            note.add_css_class("dim-label");
            right.append(&note);
        } else {
            let mut groups: Vec<(String, Vec<&TabChip>)> = Vec::new();
            for chip in &data.chips {
                match groups.iter_mut().find(|(role, _)| *role == chip.role) {
                    Some((_, list)) => list.push(chip),
                    None => groups.push((chip.role.clone(), vec![chip])),
                }
            }
            for (role, list) in &groups {
                let header = gtk::Label::new(Some(&role.to_uppercase()));
                header.set_xalign(0.0);
                header.set_margin_top(6);
                header.add_css_class("dim-label");
                header.add_css_class("caption");
                header.add_css_class("overview-role-header");
                right.append(&header);
                for chip in list {
                    right.append(&tab_row(
                        chip,
                        data.selected.as_deref() == Some(chip.uuid.as_str()),
                        &self.callbacks,
                    ));
                }
            }
            if !data.tagging_available {
                let note = gtk::Label::new(Some("Tagging unavailable (no tagger command)"));
                note.set_xalign(0.0);
                note.set_wrap(true);
                note.add_css_class("dim-label");
                note.add_css_class("caption");
                right.append(&note);
            }
        }
        let right_scroller = gtk::ScrolledWindow::new();
        right_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        right_scroller.set_propagate_natural_width(true);
        right_scroller.set_child(Some(&right));
        self.panel.append(&right_scroller);
    }
}

/// What the card panel is built from, copied out of the state so no borrow
/// is held while widgets are created.
struct PanelData {
    node: Node,
    status_color: Option<Color>,
    parents: Vec<String>,
    links: Vec<(String, String)>,
    chips: Vec<TabChip>,
    selected: Option<String>,
    tagging_available: bool,
}

fn tab_row(chip: &TabChip, selected: bool, callbacks: &Callbacks) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let glyph = gtk::Label::new(Some(role_glyph(&chip.role)));
    glyph.set_valign(gtk::Align::Start);
    row.append(&glyph);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_hexpand(true);
    let name = gtk::Label::new(None);
    name.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&chip.name)));
    name.set_xalign(0.0);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_max_width_chars(24);
    text.append(&name);
    if let Some(activity) = &chip.activity {
        let line = gtk::Label::new(Some(activity));
        line.set_xalign(0.0);
        line.set_ellipsize(pango::EllipsizeMode::End);
        line.set_max_width_chars(28);
        line.add_css_class("dim-label");
        line.add_css_class("caption");
        text.append(&line);
    }
    row.append(&text);
    let age = gtk::Label::new(Some(&chip.age));
    age.set_valign(gtk::Align::Start);
    age.add_css_class("dim-label");
    age.add_css_class("caption");
    row.append(&age);
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("overview-tab-row");
    if selected {
        button.add_css_class("selected");
    }
    button.set_child(Some(&row));
    let open_tab = callbacks.open_tab.clone();
    let uuid = chip.uuid.clone();
    button.connect_clicked(move |_| open_tab(uuid.clone()));
    button
}

fn chip_label(text: &str, color: Option<Color>) -> gtk::Label {
    let label = gtk::Label::new(None);
    match color {
        Some(c) => label.set_markup(&format!(
            "<span foreground=\"{}\">{}</span>",
            hex(&rgba(c, 1.0)),
            glib::markup_escape_text(text)
        )),
        None => label.set_text(text),
    }
    label.add_css_class("overview-chip");
    label.add_css_class("caption");
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.set_max_width_chars(40);
    label
}

fn field_text(value: &FieldValue) -> String {
    match value {
        FieldValue::Scalar(s) => s.clone(),
        FieldValue::List(items) => items.join(", "),
    }
}

fn build_tray(title: &str) -> (gtk::Box, gtk::Box) {
    let tray = gtk::Box::new(gtk::Orientation::Vertical, 4);
    tray.add_css_class("overview-tray");
    tray.set_visible(false);
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    tray.append(&heading);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);
    scroller.set_max_content_height(240);
    scroller.set_child(Some(&rows));
    tray.append(&scroller);
    (tray, rows)
}

fn clear_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn breadcrumb_markup(group: &str, crumbs: &[String]) -> String {
    let root = format!("{} overview", glib::markup_escape_text(group));
    if crumbs.is_empty() {
        return format!("<b>{root}</b>");
    }
    let mut out = root;
    let last = crumbs.len() - 1;
    for (i, crumb) in crumbs.iter().enumerate() {
        let text = glib::markup_escape_text(crumb);
        if i == last {
            out += &format!(" \u{203a} <b>{text}</b>");
        } else {
            out += &format!(" \u{203a} {text}");
        }
    }
    out
}

/// A group name as it goes on a command line.
fn shell_word(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "'\\''"))
    }
}

fn role_glyph(role: &str) -> &'static str {
    match role {
        "planning" => "\u{270e}",
        "implementing" => "\u{2692}",
        "researching" => "\u{2315}",
        "related" => "\u{26d3}",
        _ => "\u{25cf}",
    }
}

fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.replace(true) {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

// --- Markdown into a TextBuffer ---

/// Render `body` into `buffer` with tags for headings, paragraphs, lists,
/// quotes, emphasis, code and links. Each link gets its own tag
/// `link:<n>`, recorded in `links` with its href; images become their alt
/// text.
fn render_markdown(body: &str, buffer: &gtk::TextBuffer, links: &mut HashMap<String, String>) {
    let mut w = MdWriter::new(buffer);
    for event in Parser::new(body) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    w.block_break();
                    w.push("para");
                }
                Tag::Heading { level, .. } => {
                    w.block_break();
                    w.push(match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        _ => "h3",
                    });
                }
                Tag::BlockQuote(_) => {
                    w.block_break();
                    w.push("quote");
                }
                Tag::CodeBlock(_) => {
                    w.block_break();
                    w.push("codeblock");
                }
                Tag::List(start) => w.lists.push(start),
                Tag::Item => {
                    w.block_break();
                    w.push("list");
                    let marker = match w.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => "\u{2022} ".to_string(),
                    };
                    let indent = "    ".repeat(w.lists.len().saturating_sub(1));
                    w.insert(&format!("{indent}{marker}"));
                    // The item's own paragraph continues on the marker's line.
                    w.at_line_start = true;
                }
                Tag::Emphasis => w.push("em"),
                Tag::Strong => w.push("strong"),
                Tag::Strikethrough => w.push("strike"),
                Tag::Link { dest_url, .. } => {
                    let tag = w.link_tag(dest_url.to_string(), links);
                    w.stack.push(tag);
                }
                Tag::Image { .. } => w.push("em"),
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::Item => {
                    w.pop();
                    w.line_end();
                }
                TagEnd::List(_) => {
                    w.lists.pop();
                }
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image => w.pop(),
                TagEnd::TableCell => w.insert("  "),
                TagEnd::TableRow | TagEnd::TableHead => w.line_end(),
                _ => {}
            },
            Event::Text(text) => w.insert(&text),
            Event::Code(code) => {
                w.push("code");
                w.insert(&code);
                w.pop();
            }
            Event::SoftBreak => w.insert(" "),
            Event::HardBreak => w.insert("\n"),
            Event::Rule => {
                w.block_break();
                w.push("rule");
                w.insert("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n");
                w.pop();
            }
            Event::TaskListMarker(checked) => {
                w.insert(if checked { "\u{2611} " } else { "\u{2610} " })
            }
            Event::FootnoteReference(name) => w.insert(&format!("[{name}]")),
            _ => {}
        }
    }
}

struct MdWriter<'a> {
    buffer: &'a gtk::TextBuffer,
    tags: HashMap<&'static str, gtk::TextTag>,
    stack: Vec<gtk::TextTag>,
    lists: Vec<Option<u64>>,
    at_line_start: bool,
    empty: bool,
    links: usize,
}

impl<'a> MdWriter<'a> {
    fn new(buffer: &'a gtk::TextBuffer) -> MdWriter<'a> {
        let table = buffer.tag_table();
        let mut tags: HashMap<&'static str, gtk::TextTag> = HashMap::new();
        let muted = MUTED_GREY;
        let code_bg = gdk::RGBA::new(0.5, 0.5, 0.5, 0.15);
        let defs: [(&'static str, gtk::TextTag); 11] = [
            (
                "para",
                gtk::TextTag::builder()
                    .name("para")
                    .pixels_above_lines(2)
                    .pixels_below_lines(8)
                    .build(),
            ),
            (
                "h1",
                gtk::TextTag::builder()
                    .name("h1")
                    .weight(700)
                    .scale(1.6)
                    .pixels_above_lines(14)
                    .pixels_below_lines(6)
                    .build(),
            ),
            (
                "h2",
                gtk::TextTag::builder()
                    .name("h2")
                    .weight(700)
                    .scale(1.3)
                    .pixels_above_lines(12)
                    .pixels_below_lines(4)
                    .build(),
            ),
            (
                "h3",
                gtk::TextTag::builder()
                    .name("h3")
                    .weight(700)
                    .scale(1.1)
                    .pixels_above_lines(10)
                    .pixels_below_lines(3)
                    .build(),
            ),
            (
                "list",
                gtk::TextTag::builder()
                    .name("list")
                    .left_margin(18)
                    .pixels_above_lines(1)
                    .pixels_below_lines(2)
                    .build(),
            ),
            (
                "quote",
                gtk::TextTag::builder()
                    .name("quote")
                    .left_margin(16)
                    .style(pango::Style::Italic)
                    .foreground_rgba(&muted)
                    .pixels_below_lines(6)
                    .build(),
            ),
            (
                "codeblock",
                gtk::TextTag::builder()
                    .name("codeblock")
                    .family("monospace")
                    .left_margin(12)
                    .paragraph_background_rgba(&code_bg)
                    .pixels_above_lines(4)
                    .pixels_below_lines(4)
                    .build(),
            ),
            (
                "code",
                gtk::TextTag::builder()
                    .name("code")
                    .family("monospace")
                    .background_rgba(&code_bg)
                    .build(),
            ),
            (
                "em",
                gtk::TextTag::builder()
                    .name("em")
                    .style(pango::Style::Italic)
                    .build(),
            ),
            (
                "strong",
                gtk::TextTag::builder().name("strong").weight(700).build(),
            ),
            (
                "strike",
                gtk::TextTag::builder()
                    .name("strike")
                    .strikethrough(true)
                    .build(),
            ),
        ];
        for (name, tag) in defs {
            table.add(&tag);
            tags.insert(name, tag);
        }
        let rule = gtk::TextTag::builder()
            .name("rule")
            .foreground_rgba(&muted)
            .build();
        table.add(&rule);
        tags.insert("rule", rule);
        MdWriter {
            buffer,
            tags,
            stack: Vec::new(),
            lists: Vec::new(),
            at_line_start: true,
            empty: true,
            links: 0,
        }
    }

    fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let refs: Vec<&gtk::TextTag> = self.stack.iter().collect();
        let mut iter = self.buffer.end_iter();
        self.buffer.insert_with_tags(&mut iter, text, &refs);
        self.at_line_start = text.ends_with('\n');
        self.empty = false;
    }

    /// Start a block on its own line.
    fn block_break(&mut self) {
        if !self.empty && !self.at_line_start {
            self.insert("\n");
        }
    }

    /// End the current line (no-op when already at a line start).
    fn line_end(&mut self) {
        if !self.at_line_start {
            self.insert("\n");
        }
    }

    fn push(&mut self, name: &'static str) {
        if let Some(tag) = self.tags.get(name) {
            self.stack.push(tag.clone());
        }
    }

    fn pop(&mut self) {
        self.stack.pop();
    }

    fn link_tag(&mut self, href: String, links: &mut HashMap<String, String>) -> gtk::TextTag {
        self.links += 1;
        let name = format!("link:{}", self.links);
        let tag = gtk::TextTag::builder()
            .name(name.clone())
            .underline(pango::Underline::Single)
            .foreground_rgba(&LINK_BLUE)
            .build();
        self.buffer.tag_table().add(&tag);
        links.insert(name, href);
        tag
    }
}

// --- the canvas widget ---

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct OverviewCanvas {
        /// The view this canvas draws for; set once by `Inner::wire`.
        pub(super) view: RefCell<Weak<Inner>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OverviewCanvas {
        const NAME: &'static str = "KabelsalatOverviewCanvas";
        type Type = super::OverviewCanvas;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("overview-canvas");
        }
    }

    impl ObjectImpl for OverviewCanvas {}

    impl WidgetImpl for OverviewCanvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            let Some(inner) = self.view.borrow().upgrade() else {
                return;
            };
            let changed = {
                let mut s = inner.state.borrow_mut();
                let (w, h) = (f64::from(width), f64::from(height));
                if differs(s.viewport.w, w) || differs(s.viewport.h, h) {
                    s.viewport.w = w;
                    s.viewport.h = h;
                    if s.needs_fit && w > 0.0 && h > 0.0 {
                        s.needs_fit = false;
                        let bounds = s.layout.bounds;
                        s.viewport = s.viewport.fit(&bounds, FIT_MARGIN);
                    }
                    true
                } else {
                    false
                }
            };
            if changed {
                // Not during allocation: the slider, breadcrumb and panel
                // follow on the next main-loop iteration.
                glib::idle_add_local_once(move || inner.viewport_changed());
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(inner) = self.view.borrow().upgrade() else {
                return;
            };
            let mut s = inner.state.borrow_mut();
            let widget = self.obj();
            draw_scene(widget.upcast_ref::<gtk::Widget>(), snapshot, &mut s);
        }
    }
}

glib::wrapper! {
    pub struct OverviewCanvas(ObjectSubclass<imp::OverviewCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl OverviewCanvas {
    fn new() -> Self {
        glib::Object::new()
    }
}

// --- drawing ---

/// Colours derived from the theme foreground so the map reads in dark
/// themes too.
struct Theme {
    fg: gdk::RGBA,
    muted: gdk::RGBA,
    border: gdk::RGBA,
    paper: gdk::RGBA,
    card: gdk::RGBA,
    row: gdk::RGBA,
    chip: gdk::RGBA,
    grid: gdk::RGBA,
    edge: gdk::RGBA,
}

impl Theme {
    fn from_fg(fg: gdk::RGBA) -> Theme {
        let lum = 0.2126 * fg.red() + 0.7152 * fg.green() + 0.0722 * fg.blue();
        let dark = lum > 0.5;
        let (paper, card, row) = if dark {
            (
                gdk::RGBA::new(1.0, 1.0, 1.0, 0.06),
                gdk::RGBA::new(1.0, 1.0, 1.0, 0.10),
                gdk::RGBA::new(1.0, 1.0, 1.0, 0.07),
            )
        } else {
            (
                gdk::RGBA::new(1.0, 1.0, 1.0, 0.85),
                gdk::RGBA::new(1.0, 1.0, 1.0, 1.0),
                gdk::RGBA::new(0.0, 0.0, 0.0, 0.045),
            )
        };
        Theme {
            fg,
            muted: alpha(&fg, 0.6),
            border: alpha(&fg, 0.28),
            paper,
            card,
            row,
            chip: alpha(&fg, 0.09),
            grid: alpha(&fg, 0.16),
            edge: alpha(&fg, 0.45),
        }
    }
}

#[derive(Clone, Copy)]
struct Font {
    px: f64,
    bold: bool,
    strike: bool,
}

impl Font {
    fn plain(px: f64) -> Font {
        Font {
            px,
            bold: false,
            strike: false,
        }
    }

    fn bold(px: f64) -> Font {
        Font {
            px,
            bold: true,
            strike: false,
        }
    }
}

/// One frame's drawing helpers.
struct Draw<'a> {
    widget: &'a gtk::Widget,
    snapshot: &'a gtk::Snapshot,
    theme: &'a Theme,
    vp: Viewport,
    tier: Tier,
    canvas: Rect,
    header_h: f64,
}

impl Draw<'_> {
    fn scale(&self) -> f64 {
        self.vp.scale
    }

    /// A screen size for a world size, never below the readable minimum.
    fn text_px(&self, world_px: f64) -> f64 {
        (world_px * self.vp.scale).max(MIN_TEXT_PX)
    }

    fn cairo(&self, bounds: &Rect) -> cairo::Context {
        self.snapshot.append_cairo(&grect(&grow(bounds, 2.0)))
    }

    fn layout_for(&self, text: &str, font: Font, max_w: f64) -> pango::Layout {
        let layout = self.widget.create_pango_layout(None);
        if font.strike {
            layout.set_markup(&format!("<s>{}</s>", glib::markup_escape_text(text)));
        } else {
            layout.set_text(text);
        }
        let mut fd = pango::FontDescription::new();
        fd.set_absolute_size(font.px * f64::from(pango::SCALE));
        if font.bold {
            fd.set_weight(pango::Weight::Bold);
        }
        layout.set_font_description(Some(&fd));
        layout.set_single_paragraph_mode(true);
        if max_w > 0.0 {
            layout.set_width((max_w * f64::from(pango::SCALE)) as i32);
            layout.set_ellipsize(pango::EllipsizeMode::End);
        }
        layout
    }

    fn place(&self, layout: &pango::Layout, x: f64, y: f64, color: &gdk::RGBA) {
        self.snapshot.save();
        self.snapshot
            .translate(&graphene::Point::new(x as f32, y as f32));
        self.snapshot.append_layout(layout, color);
        self.snapshot.restore();
    }

    /// Draw one line of text, ellipsized to `max_w`; returns its size.
    fn text(
        &self,
        text: &str,
        x: f64,
        y: f64,
        max_w: f64,
        font: Font,
        color: &gdk::RGBA,
    ) -> (f64, f64) {
        if max_w < 6.0 || text.is_empty() {
            return (0.0, 0.0);
        }
        let layout = self.layout_for(text, font, max_w);
        let (w, h) = layout.pixel_size();
        self.place(&layout, x, y, color);
        (f64::from(w), f64::from(h))
    }

    fn pill_size(&self, text: &str, font: Font) -> (f64, f64) {
        let layout = self.layout_for(text, font, -1.0);
        let (w, h) = layout.pixel_size();
        let (hpad, vpad) = pill_padding(font.px);
        (f64::from(w) + 2.0 * hpad, f64::from(h) + 2.0 * vpad)
    }

    /// A rounded pill with `text` at `(x, y)`; returns its size.
    fn pill(
        &self,
        text: &str,
        x: f64,
        y: f64,
        font: Font,
        fill: &gdk::RGBA,
        color: &gdk::RGBA,
    ) -> (f64, f64) {
        let layout = self.layout_for(text, font, -1.0);
        let (w, h) = layout.pixel_size();
        let (hpad, vpad) = pill_padding(font.px);
        let rect = Rect {
            x,
            y,
            w: f64::from(w) + 2.0 * hpad,
            h: f64::from(h) + 2.0 * vpad,
        };
        let cr = self.cairo(&rect);
        rounded_path(&cr, &rect, rect.h / 2.0);
        set_source(&cr, fill);
        let _ = cr.fill();
        self.place(&layout, x + hpad, y + vpad, color);
        (rect.w, rect.h)
    }
}

fn pill_padding(px: f64) -> (f64, f64) {
    ((px * 0.6).max(4.0), (px * 0.18).max(1.5))
}

fn draw_scene(widget: &gtk::Widget, snapshot: &gtk::Snapshot, s: &mut State) {
    let vp = s.viewport;
    if vp.w <= 0.0 || vp.h <= 0.0 {
        return;
    }
    let canvas = Rect {
        x: 0.0,
        y: 0.0,
        w: vp.w,
        h: vp.h,
    };
    let theme = Theme::from_fg(widget.color());
    snapshot.push_clip(&grect(&canvas));
    draw_grid(snapshot, &canvas, &vp, &theme);
    s.hits.clear();
    s.drawn.clear();
    let graph = s.graph.clone();
    let layout = s.layout.clone();
    let ctx = Draw {
        widget,
        snapshot,
        theme: &theme,
        vp,
        tier: vp.tier(),
        canvas,
        header_h: layout::Metrics::default().header_h,
    };
    for b in layout::visible(&layout, ctx.tier) {
        match (ctx.tier, b.level) {
            (_, 0) => draw_container(&ctx, s, &graph, &layout, b),
            (Tier::Near | Tier::Close, 1) => draw_card(&ctx, s, &graph, &layout, b),
            // Rows are drawn by their container.
            _ => {}
        }
    }
    draw_edges(&ctx, s, &graph, &layout);
    snapshot.pop();
}

fn draw_grid(snapshot: &gtk::Snapshot, canvas: &Rect, vp: &Viewport, theme: &Theme) {
    let mut step = GRID_STEP * vp.scale;
    if !step.is_finite() || step <= 0.0 {
        return;
    }
    while step < GRID_MIN_PX {
        step *= 2.0;
    }
    let cr = snapshot.append_cairo(&grect(canvas));
    set_source(&cr, &theme.grid);
    let x0 = vp.ox.rem_euclid(step);
    let y0 = vp.oy.rem_euclid(step);
    let mut x = x0;
    while x < canvas.w {
        let mut y = y0;
        while y < canvas.h {
            cr.rectangle(x - 0.5, y - 0.5, 1.0, 1.0);
            y += step;
        }
        x += step;
    }
    let _ = cr.fill();
}

/// The level-(n+1) boxes placed inside one container copy, in child order.
fn rows_of<'a>(layout: &'a Layout, container: &PlacedBox) -> Vec<&'a PlacedBox> {
    let mut rows: Vec<&PlacedBox> = layout
        .boxes
        .iter()
        .filter(|b| {
            let (cx, cy) = b.rect.center();
            b.level == container.level + 1
                && b.key.parent.as_deref() == Some(container.key.id.as_str())
                && container.rect.contains(cx, cy)
        })
        .collect();
    rows.sort_by_key(|b| b.child_order);
    rows
}

fn record_drawn(s: &mut State, b: &PlacedBox, rect: Rect) {
    if !b.mirror || !s.drawn.contains_key(&b.key) {
        s.drawn.insert(b.key.clone(), rect);
    }
}

/// The outline shared by containers and cards: fill, optional kind stripe,
/// border (dashed for mirrors, accent when hovered).
fn box_shape(
    ctx: &Draw,
    r: &Rect,
    fill: &gdk::RGBA,
    stripe: Option<Color>,
    mirror: bool,
    hovered: bool,
) {
    let sc = ctx.scale();
    let radius = (BOX_RADIUS * sc).max(3.0);
    let cr = ctx.cairo(r);
    rounded_path(&cr, r, radius);
    set_source(&cr, fill);
    let _ = cr.fill_preserve();
    if let Some(kind) = stripe {
        let _ = cr.save();
        cr.clip();
        set_source(&cr, &rgba(kind, 1.0));
        cr.rectangle(r.x, r.y, r.w, (STRIPE_H * sc).max(2.0));
        let _ = cr.fill();
        let _ = cr.restore();
        rounded_path(&cr, r, radius);
    }
    if hovered {
        set_source(&cr, &ACCENT);
        cr.set_line_width(2.0);
    } else {
        set_source(&cr, &ctx.theme.border);
        cr.set_line_width(1.0);
    }
    if mirror {
        cr.set_dash(&[5.0, 4.0], 0.0);
    }
    let _ = cr.stroke();
}

/// The status bar: one segment per status colour, by share.
fn status_bar(ctx: &Draw, statuses: &[(Color, usize)], x: f64, y: f64, w: f64, h: f64) {
    let total: usize = statuses.iter().map(|(_, n)| n).sum();
    if total == 0 || w <= 2.0 {
        return;
    }
    let bar = Rect { x, y, w, h };
    let cr = ctx.cairo(&bar);
    rounded_path(&cr, &bar, h / 2.0);
    cr.clip();
    let mut sx = x;
    for (color, n) in statuses {
        let seg = w * (*n as f64) / (total as f64);
        set_source(&cr, &rgba(*color, 1.0));
        cr.rectangle(sx, y, seg, h);
        let _ = cr.fill();
        sx += seg;
    }
}

/// The pills at a box's top-right corner: the mirror mark, "● N tabs",
/// "⚠ N". Returns the x where the title must end.
fn header_pills(
    ctx: &Draw,
    summary: &Summary,
    mirror: bool,
    right: f64,
    y: f64,
    font: Font,
) -> f64 {
    let mut right = right;
    let gap = (4.0 * ctx.scale()).max(3.0);
    if mirror {
        let (w, _) = ctx.pill_size("\u{25c7}", font);
        right -= w;
        ctx.pill(
            "\u{25c7}",
            right,
            y,
            font,
            &ctx.theme.chip,
            &ctx.theme.muted,
        );
        right -= gap;
    }
    if summary.issues > 0 {
        let text = format!("\u{26a0} {}", summary.issues);
        let (w, _) = ctx.pill_size(&text, font);
        right -= w;
        ctx.pill(&text, right, y, font, &alpha(&AMBER, 0.22), &AMBER);
        right -= gap;
    }
    if summary.tabs > 0 {
        let text = format!("\u{25cf} {} tabs", summary.tabs);
        let (w, _) = ctx.pill_size(&text, font);
        right -= w;
        ctx.pill(&text, right, y, font, &alpha(&GREEN, 0.18), &GREEN);
        right -= gap;
    }
    right
}

fn draw_container(ctx: &Draw, s: &mut State, graph: &Graph, layout: &Layout, b: &PlacedBox) {
    let r = screen_rect(&ctx.vp, &b.rect);
    if !intersects(&r, &ctx.canvas) {
        return;
    }
    record_drawn(s, b, r);
    let Some(node) = graph.node(&b.key.id) else {
        return;
    };
    let kind = graph.kind_color(&node.kind);
    let hovered = s.hover.as_deref() == Some(b.key.id.as_str());
    let summary = s.summary(&b.key.id);
    let sc = ctx.scale();
    let pad = PAD * sc;

    box_shape(ctx, &r, &ctx.theme.paper, Some(kind), b.mirror, hovered);
    if ctx.tier >= Tier::Near {
        // A tinted header band behind the title.
        let band = Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: (ctx.header_h * sc).min(r.h),
        };
        let cr = ctx.cairo(&band);
        rounded_path(&cr, &r, (BOX_RADIUS * sc).max(3.0));
        cr.clip();
        set_source(&cr, &rgba(kind, 0.12));
        cr.rectangle(band.x, band.y + (STRIPE_H * sc).max(2.0), band.w, band.h);
        let _ = cr.fill();
    }

    let x = r.x + pad;
    let mut y = r.y + (STRIPE_H * sc).max(2.0) + (6.0 * sc).max(3.0);
    let right = r.x + r.w - pad;
    if r.h < MIN_TEXT_PX * 2.0 || r.w < MIN_TEXT_PX * 4.0 {
        return;
    }
    let pill_font = Font::plain(ctx.text_px(11.0));
    let title_end = header_pills(ctx, &summary, b.mirror, right, y, pill_font);
    let (_, th) = ctx.text(
        &node.title,
        x,
        y,
        title_end - x - 4.0,
        Font::bold(ctx.text_px(16.0)),
        &ctx.theme.fg,
    );
    y += th + (2.0 * sc).max(1.0);
    let meta = if summary.nodes == 1 {
        format!("1 node \u{00b7} {}", node.kind)
    } else {
        format!("{} nodes \u{00b7} {}", summary.nodes, node.kind)
    };
    if y + MIN_TEXT_PX < r.y + r.h {
        let (_, mh) = ctx.text(
            &meta,
            x,
            y,
            right - x,
            Font::plain(ctx.text_px(12.0)),
            &ctx.theme.muted,
        );
        y += mh + (4.0 * sc).max(2.0);
    }

    match ctx.tier {
        Tier::Far => {
            let bar_h = (6.0 * sc).max(3.0);
            let bar_y = r.y + r.h - pad - bar_h;
            if bar_y > y {
                status_bar(ctx, &summary.statuses, x, bar_y, right - x, bar_h);
            }
        }
        Tier::Mid => {
            draw_rows(ctx, s, graph, layout, b, &r, y);
        }
        Tier::Near | Tier::Close => {
            let bar_h = (3.0 * sc).max(2.0);
            let bar_y = r.y + ctx.header_h * sc - bar_h - (4.0 * sc).max(2.0);
            if bar_y > y {
                status_bar(ctx, &summary.statuses, x, bar_y, right - x, bar_h);
            }
        }
    }
}

/// One-line rows for a container's children: status dot, title (struck
/// through when the status is grey), tab count; capped at `MID_ROWS` or the
/// available height, then "+ N more".
fn draw_rows(
    ctx: &Draw,
    s: &mut State,
    graph: &Graph,
    layout: &Layout,
    container: &PlacedBox,
    r: &Rect,
    top: f64,
) {
    let rows = rows_of(layout, container);
    if rows.is_empty() {
        return;
    }
    let sc = ctx.scale();
    let row_h = (ROW_H * sc).max(MIN_ROW_PX);
    let pad = PAD * sc;
    let bottom = r.y + r.h - pad;
    let font = Font::plain(ctx.text_px(12.0));
    let x = r.x + pad;
    let w = r.w - 2.0 * pad;
    let mut y = top;
    let mut shown = 0;
    for child in rows.iter().take(MID_ROWS) {
        if y + row_h > bottom {
            break;
        }
        let row = Rect {
            x,
            y,
            w,
            h: row_h - (2.0 * sc).max(1.0),
        };
        record_drawn(s, child, row);
        let hovered = s.hover.as_deref() == Some(child.key.id.as_str());
        let fill = if hovered {
            alpha(&ACCENT, 0.18)
        } else {
            ctx.theme.row
        };
        let cr = ctx.cairo(&row);
        rounded_path(&cr, &row, (6.0 * sc).max(2.0));
        set_source(&cr, &fill);
        let _ = cr.fill_preserve();
        if child.mirror {
            set_source(&cr, &ctx.theme.border);
            cr.set_dash(&[4.0, 3.0], 0.0);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
        }
        let Some(node) = graph.node(&child.key.id) else {
            y += row_h;
            continue;
        };
        let status = graph.status_color(node);
        let dot = (3.5 * sc).max(2.5);
        let cy = row.y + row.h / 2.0;
        cr.new_path();
        cr.arc(row.x + dot + (6.0 * sc).max(3.0), cy, dot, 0.0, 2.0 * PI);
        let dot_color = match status {
            Some(c) => rgba(c, 1.0),
            None => alpha(&ctx.theme.fg, 0.3),
        };
        set_source(&cr, &dot_color);
        let _ = cr.fill();
        let text_x = row.x + 2.0 * dot + (12.0 * sc).max(6.0);
        let mut text_right = row.x + row.w - (6.0 * sc).max(3.0);
        let tabs = s.summary(&child.key.id).tabs;
        if tabs > 0 {
            let count = format!("\u{25cf} {tabs}");
            let (cw, _) = ctx.pill_size(&count, font);
            text_right -= cw;
            ctx.pill(
                &count,
                text_right,
                cy - font.px * 0.75,
                font,
                &alpha(&GREEN, 0.18),
                &GREEN,
            );
            text_right -= (4.0 * sc).max(2.0);
        }
        let title = if child.mirror {
            format!("{} \u{25c7}", node.title)
        } else {
            node.title.clone()
        };
        let row_font = Font {
            strike: status == Some(Color::Grey),
            ..font
        };
        ctx.text(
            &title,
            text_x,
            cy - font.px * 0.7,
            text_right - text_x,
            row_font,
            &ctx.theme.fg,
        );
        y += row_h;
        shown += 1;
    }
    let hidden = rows.len() - shown;
    if hidden > 0 && y + MIN_TEXT_PX <= bottom {
        ctx.text(
            &format!("+ {hidden} more"),
            x + (8.0 * sc).max(4.0),
            y,
            w,
            Font::plain(ctx.text_px(11.0)),
            &ctx.theme.muted,
        );
    }
}

/// A level-1 card at Near/Close: title, summary, status chip and tab chips
/// (collapsed to "● N tabs" on a mirror); with children, a compact header
/// and the children as rows.
fn draw_card(ctx: &Draw, s: &mut State, graph: &Graph, layout: &Layout, b: &PlacedBox) {
    let r = screen_rect(&ctx.vp, &b.rect);
    if !intersects(&r, &ctx.canvas) {
        return;
    }
    record_drawn(s, b, r);
    let Some(node) = graph.node(&b.key.id) else {
        return;
    };
    let hovered = s.hover.as_deref() == Some(b.key.id.as_str());
    let sc = ctx.scale();
    let pad = CARD_PAD * sc;
    box_shape(ctx, &r, &ctx.theme.card, None, b.mirror, hovered);
    if r.h < MIN_TEXT_PX * 2.0 || r.w < MIN_TEXT_PX * 4.0 {
        return;
    }
    let has_rows = !rows_of(layout, b).is_empty();
    let x = r.x + pad;
    let right = r.x + r.w - pad;
    let mut y = r.y + (8.0 * sc).max(4.0);
    let title = if b.mirror {
        format!("{} \u{25c7}", node.title)
    } else {
        node.title.clone()
    };
    let (_, th) = ctx.text(
        &title,
        x,
        y,
        right - x,
        Font::bold(ctx.text_px(13.0)),
        &ctx.theme.fg,
    );
    y += th + (3.0 * sc).max(1.0);

    let chip_font = Font::plain(ctx.text_px(10.0));
    let status = graph.status_color(node);
    let chips: Vec<TabChip> = s
        .tabs_by_node
        .get(&b.key.id)
        .map(|idx| {
            idx.iter()
                .filter_map(|i| s.chips.get(*i).cloned())
                .collect()
        })
        .unwrap_or_default();
    let limit = if has_rows {
        r.y + ctx.header_h * sc
    } else {
        r.y + r.h - pad
    };

    if !has_rows && let Some(summary) = &node.summary {
        let (_, sh) = ctx.text(
            summary,
            x,
            y,
            right - x,
            Font::plain(ctx.text_px(11.0)),
            &ctx.theme.muted,
        );
        y += sh + (4.0 * sc).max(2.0);
    }

    // Status chip and tab chips flow left to right, wrapping while there is
    // room; a mirror collapses its tabs to one count pill.
    let mut cx = x;
    let gap = (5.0 * sc).max(3.0);
    let mut chip_h = 0.0_f64;
    let mut place_chip = |ctx: &Draw,
                          text: &str,
                          fill: &gdk::RGBA,
                          color: &gdk::RGBA,
                          y: &mut f64|
     -> Option<Rect> {
        let (w, h) = ctx.pill_size(text, chip_font);
        if cx + w > right && cx > x {
            cx = x;
            *y += chip_h + gap;
        }
        if *y + h > limit {
            return None;
        }
        ctx.pill(text, cx, *y, chip_font, fill, color);
        let rect = Rect { x: cx, y: *y, w, h };
        cx += w + gap;
        chip_h = h;
        Some(rect)
    };
    if let (Some(text), Some(color)) = (&node.status, status) {
        let _ = place_chip(ctx, text, &rgba(color, 0.18), &rgba(color, 1.0), &mut y);
    }
    if b.mirror {
        if !chips.is_empty() {
            let distinct: HashSet<&str> = chips.iter().map(|c| c.uuid.as_str()).collect();
            let text = format!("\u{25cf} {} tabs", distinct.len());
            let _ = place_chip(ctx, &text, &alpha(&GREEN, 0.18), &GREEN, &mut y);
        }
    } else {
        let selected = s.selected.clone();
        for chip in &chips {
            let text = format!("{} {}", role_glyph(&chip.role), chip.name);
            let is_selected = selected.as_deref() == Some(chip.uuid.as_str());
            let fill = if is_selected { MINT } else { ctx.theme.chip };
            match place_chip(ctx, &text, &fill, &ctx.theme.fg, &mut y) {
                Some(rect) => s.hits.push(Hit {
                    rect,
                    uuid: chip.uuid.clone(),
                }),
                None => break,
            }
        }
    }

    if has_rows {
        let top = r.y + ctx.header_h * sc + (4.0 * sc).max(2.0);
        draw_rows(ctx, s, graph, layout, b, &r, top);
    }
}

/// Curves between the drawn boxes: bundled links (arrowheads and label
/// pills from Near on) and dashed amber warning edges (Near on, no
/// arrowhead).
fn draw_edges(ctx: &Draw, s: &State, graph: &Graph, layout: &Layout) {
    let edges = layout::edges(graph, layout, ctx.tier, &s.warning_pairs);
    if edges.is_empty() {
        return;
    }
    let near = ctx.tier >= Tier::Near;
    let sc = ctx.scale();
    let cr = ctx.cairo(&ctx.canvas);
    cr.set_line_cap(cairo::LineCap::Round);
    let mut labels: Vec<(String, (f64, f64))> = Vec::new();
    for e in &edges {
        if e.warning && !near {
            continue;
        }
        let (Some(a), Some(b)) = (s.drawn.get(&e.from), s.drawn.get(&e.to)) else {
            continue;
        };
        let [p0, p1, p2, p3] = edge_curve(a, b);
        cr.new_path();
        cr.move_to(p0.0, p0.1);
        cr.curve_to(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1);
        if e.warning {
            set_source(&cr, &alpha(&AMBER, 0.9));
            cr.set_line_width(1.5);
            cr.set_dash(&[6.0, 4.0], 0.0);
        } else {
            set_source(&cr, &ctx.theme.edge);
            cr.set_line_width(if near {
                1.4
            } else {
                (2.4 * sc).clamp(1.2, 3.0)
            });
            cr.set_dash(&[], 0.0);
        }
        let _ = cr.stroke();
        if near && !e.warning {
            let (dx, dy) = (p3.0 - p2.0, p3.1 - p2.1);
            let len = (dx * dx + dy * dy).sqrt();
            if len > 0.001 {
                let (ux, uy) = (dx / len, dy / len);
                let (bx, by) = (p3.0 - ux * ARROW_PX, p3.1 - uy * ARROW_PX);
                let (nx, ny) = (-uy * ARROW_PX * 0.45, ux * ARROW_PX * 0.45);
                cr.new_path();
                cr.move_to(p3.0, p3.1);
                cr.line_to(bx + nx, by + ny);
                cr.line_to(bx - nx, by - ny);
                cr.close_path();
                let _ = cr.fill();
            }
        }
        let label = if e.warning || ctx.tier == Tier::Mid {
            None
        } else if e.count == 1 {
            e.labels.first().cloned()
        } else {
            Some(format!("{} links", e.count))
        };
        if let Some(text) = label.filter(|t| !t.is_empty()) {
            labels.push((text, bezier_mid(p0, p1, p2, p3)));
        }
    }
    let font = Font::plain(ctx.text_px(10.5));
    for (text, (mx, my)) in labels {
        let (w, h) = ctx.pill_size(&text, font);
        let fill = alpha(&ctx.theme.paper, 0.95);
        ctx.pill(
            &text,
            mx - w / 2.0,
            my - h / 2.0,
            font,
            &fill,
            &ctx.theme.muted,
        );
    }
}

/// Anchor points on the facing sides of two boxes and the control points of
/// the cubic between them.
fn edge_curve(a: &Rect, b: &Rect) -> [(f64, f64); 4] {
    let (acx, acy) = a.center();
    let (bcx, bcy) = b.center();
    let dx = bcx - acx;
    let dy = bcy - acy;
    if dx.abs() > dy.abs() {
        let (x0, x3) = if dx > 0.0 {
            (a.x + a.w, b.x)
        } else {
            (a.x, b.x + b.w)
        };
        let off = ((x3 - x0).abs() * 0.5).max(20.0) * dx.signum();
        [(x0, acy), (x0 + off, acy), (x3 - off, bcy), (x3, bcy)]
    } else {
        let (y0, y3) = if dy > 0.0 {
            (a.y + a.h, b.y)
        } else {
            (a.y, b.y + b.h)
        };
        let off = ((y3 - y0).abs() * 0.5).max(20.0) * dy.signum();
        [(acx, y0), (acx, y0 + off), (bcx, y3 - off), (bcx, y3)]
    }
}

fn bezier_mid(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) -> (f64, f64) {
    (
        0.125 * p0.0 + 0.375 * p1.0 + 0.375 * p2.0 + 0.125 * p3.0,
        0.125 * p0.1 + 0.375 * p1.1 + 0.375 * p2.1 + 0.125 * p3.1,
    )
}

// --- small helpers ---

fn screen_rect(vp: &Viewport, r: &Rect) -> Rect {
    let (x, y) = vp.to_screen(r.x, r.y);
    Rect {
        x,
        y,
        w: r.w * vp.scale,
        h: r.h * vp.scale,
    }
}

fn grow(r: &Rect, d: f64) -> Rect {
    Rect {
        x: r.x - d,
        y: r.y - d,
        w: r.w + 2.0 * d,
        h: r.h + 2.0 * d,
    }
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

fn grect(r: &Rect) -> graphene::Rect {
    graphene::Rect::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
}

fn differs(a: f64, b: f64) -> bool {
    (a - b).abs() > 1e-9
}

fn rgba(c: Color, a: f32) -> gdk::RGBA {
    let (r, g, b) = c.rgb();
    gdk::RGBA::new(r, g, b, a)
}

fn alpha(c: &gdk::RGBA, a: f32) -> gdk::RGBA {
    gdk::RGBA::new(c.red(), c.green(), c.blue(), a)
}

fn hex(c: &gdk::RGBA) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(c.red()),
        byte(c.green()),
        byte(c.blue())
    )
}

fn set_source(cr: &cairo::Context, c: &gdk::RGBA) {
    cr.set_source_rgba(
        f64::from(c.red()),
        f64::from(c.green()),
        f64::from(c.blue()),
        f64::from(c.alpha()),
    );
}

fn rounded_path(cr: &cairo::Context, r: &Rect, radius: f64) {
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(r.x + r.w - rad, r.y + rad, rad, -FRAC_PI_2, 0.0);
    cr.arc(r.x + r.w - rad, r.y + r.h - rad, rad, 0.0, FRAC_PI_2);
    cr.arc(r.x + rad, r.y + r.h - rad, rad, FRAC_PI_2, PI);
    cr.arc(r.x + rad, r.y + rad, rad, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}
