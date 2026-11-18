//! The model of a Dreamer app package (spec 12.2.1, 13.3): its manifest,
//! its data and selection, its form, and running its commands against a
//! daemon. Presentations draw the same model: the pane app
//! (`dreamer-termapp`, Ratatui) and the desktop's native panel.

mod agent;
pub mod bound;
pub mod embeds;
pub mod installed;
pub mod manifest;
mod model;
mod process;
mod run;
pub mod session;

pub use model::{
    AgentState, App, Entry, Event, EventKind, Link, ManifestCache, Record, Said, TRANSCRIPT, Taken,
    clean_text, text,
};
pub use process::{
    Engine, EngineOut, EngineStart, Preview, Stamp, allowed, package_stamp, preview_process,
    replay_process, run_process, run_process_with, script_js,
};
pub use run::{Endpoint, Input, Session, check_module, load, preview, replay, run, run_with};
