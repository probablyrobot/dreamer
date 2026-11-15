//! The model of a Dreamer app package (spec 12.2.1, 13.3): its manifest,
//! its data and selection, its form, and running its commands against a
//! daemon. Presentations draw the same model: the pane app
//! (`dreamer-termapp`, Ratatui) and the desktop's native panel.

pub mod manifest;
mod model;
mod run;

pub use model::{App, Link, text};
pub use run::{Endpoint, Input, check_module, load, run};
