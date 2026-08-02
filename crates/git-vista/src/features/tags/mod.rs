//! The tag list (M2.21b, #236): the pure display decisions behind the Tags
//! section of the Activity panel.
//!
//! There is no `signals.rs` here on purpose — the list holds no state of its
//! own. It is a `create_local_resource` in `activity.rs` keyed on the panel's
//! visibility and the graph epoch, exactly like the event feed beside it, so a
//! tag created or deleted from the app refreshes the list with everything
//! else and there is no second copy of "is the panel open" to fall out of sync.

pub mod core;
