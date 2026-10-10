//! Overview mode: a zoomable map of a group's work, read from Markdown node
//! files with YAML frontmatter below the group's overview root. The pure
//! modules (`model`, `layout`, `tagger`, `issues`) are unit-tested; `canvas`
//! is the GTK widget and is wiring only.

pub mod canvas;
pub mod issues;
pub mod layout;
pub mod model;
pub mod tagger;
