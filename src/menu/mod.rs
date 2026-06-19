//! Top-level UI views: the main menu and any future pre-gameplay screens.
//!
//! Owns [`main_menu::GameState`], the app-wide flag used by every other
//! plugin to gate its startup and per-frame work behind
//! [`main_menu::GameState::InGame`], and the loading overlay shown during
//! [`main_menu::GameState::Loading`].

pub mod loading_screen;
pub mod main_menu;
