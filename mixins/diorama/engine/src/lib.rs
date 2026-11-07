//! Diorama's map engine for Horizon: draws the world at an angle, like a little model of it on a
//! table. Pure Rust, no game types: the game glue feeds it chunk snapshots and block looks, and
//! draws the images it composes.
//!
//! Ported from Diorama (MIT, SunkenInTime): <https://github.com/SunkenInTime/diorama>.

pub mod arrow;
pub mod baker;
pub mod bakery;
pub mod chrome;
pub mod chunk;
pub mod depth;
pub mod fog;
pub mod lighting;
pub mod looks;
pub mod map_ui;
pub mod menu;
pub mod night;
pub mod pin_art;
pub mod projection;
pub mod radar;
pub mod reader;
pub mod session;
pub mod share;
pub mod source;
pub mod store;
pub mod style;
pub mod tile;
pub mod view;
pub mod waypoint;
pub mod waypoint_ui;
pub mod worldkey;
