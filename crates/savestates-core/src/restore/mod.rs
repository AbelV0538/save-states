//! Restoring a saved state: [`plan`] it against what is open, let the user confirm,
//! then [`execute`] it.

mod execute;
mod plan;

pub use execute::{RestoreReport, execute};
pub use plan::{ExtraWindow, LaunchAction, LaunchUnit, RestorePlan, plan};
