# Sidebar tab order: activity or manual

The sidebar orders a group's tabs by last activity, most recent on top. A
toggle switches to a manual order in which new tabs land on top of their
group and drag-and-drop rearranges freely. The choice persists.

## Background

Sidebar order was purely positional: `rebuild_list` (`src/app.rs`) walked
`App.tabs` filtered per group, `add_tab` pushed to the end, and `SavedTab`
documents that "order within the containing vec is the display order".
Activity is already tracked per tab (`Tab.last_activity`, stamped on Enter,
Escape, title changes and, since 2026-09-14, any visible output; persisted as `SavedTab.last_activity`) and shown
as an age prefix that a 30-second tick repaints in place.

## Design

- `state::SidebarOrder { Activity (default), Manual }`, persisted as
  `SavedState.sidebar_order` with `serde(default)` so older files load as
  Activity.
- `state::activity_order(stamps)` is the pure, tested sort: indices by stamp
  descending, stable.
- `App::group_members(group)` is the single source of a group's display
  order for both the sidebar and the tab bar. Manual returns vec order;
  Activity applies `activity_order` and leaves the vec untouched, so
  persistence and drag-and-drop keep working on the vec.
- `rebuild_list` records the rendered id order in `shown_order`.
  `resort_if_stale` recomputes it and rebuilds only when it differs. It runs
  after every age tick and after every title change, so a tab that saw
  activity floats up within seconds without a rebuild per keystroke.
- A `ToggleButton` in the sidebar header (sort icon) sends
  `Msg::SetSidebarOrder`; it is `#[watch]`-bound to the model so restore
  reflects the persisted choice.
- Manual mode keeps the newest-first insertion: `newest_first_index` and
  `move_tab_to_group_front` place a user- or CLI-created tab before its
  group's first tab. Restore still pushes in saved order.

## Non-goals

- Drag-and-drop stays enabled in Activity mode: moving a tab to another group
  still works; the drop position within a group has no visible effect.
- No migration of existing state files.
