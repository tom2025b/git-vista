//! `git-vista-core` — pure, UI-independent logic for git-vista.
//!
//! Nothing in this crate knows about Tauri, Leptos, or any rendering. It takes a
//! repository path and produces a fully laid-out [`model::Graph`] that a frontend
//! can draw. Three small layers, each independently testable:
//!
//! - [`model`]  — serializable data types shared across the Tauri IPC boundary.
//! - [`repo`]   — reads commit history from a git repository (stub for now).
//! - [`layout`] — assigns commits to lanes for the vertical graph.

pub mod layout;
pub mod model;
pub mod repo;
