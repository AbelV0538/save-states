//! Save States core: capture the open desktop session into a [`model::SaveState`] and
//! restore it later. No UI here; `savestates-app` drives it.

pub mod capture;
pub mod chrome;
pub mod desktops;
pub mod model;
pub mod paths;
pub mod restore;
pub mod screenshot;
pub mod store;
pub mod win;
