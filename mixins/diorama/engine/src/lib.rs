//! Diorama's map engine for Horizon: draws the world at an angle, like a little model of it on a
//! table. Pure Rust, no game types: the game glue feeds it chunk snapshots and block looks, and
//! draws the images it composes.
//!
//! Ported from Diorama (MIT, SunkenInTime): <https://github.com/SunkenInTime/diorama>.

pub mod baker;
pub mod bakery;
pub mod chunk;
pub mod lighting;
pub mod looks;
pub mod projection;
pub mod reader;
pub mod session;
pub mod source;
pub mod store;
pub mod style;
pub mod tile;
pub mod view;
pub mod waypoint;
