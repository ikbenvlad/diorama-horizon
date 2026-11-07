//! Right-click menu on the fullscreen map for teleportation and waypoint management.

use crate::tile::{self, TileKey};

// Helper trait for absolute on types that don't have it
trait AbsExt<T> {
    fn absolute(self) -> T;
}

impl<T> AbsExt<T> for T
where
    T: Copy + Ord,
{
    fn absolute(self) -> T {
        if self < T::default() {
            T::zero() - self
        } else {
            self
        }
    }
}

/// What a target can be: a spot under the mouse (no waypoint there), or a nearby waypoint pin.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Block position under the mouse, with optional height information. The height may come from
    /// the tile's saved spot data (when the chunks were originally loaded) or from walking right
    /// down into the ground.
    #[allow(dead_code)]
    Spot { u: i64, v: i64, layer: i32, y: Option<i32> },
    /// A saved waypoint on the map. The pin stores the instantaneuous position where the click
    /// landed, along with the full minimized pitch.
    #[allow(dead_code)]
    Pin { u: i64, v: i64, layer: i32, pitch: f32 },
}

/// What happens when a target is clicked.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Teleport the player to this target's position.
    Teleport { yaw: f32 },
    /// Add a waypoint here (on the surface, always). Teleports first, adds waypoint.
    AddWaypoint { name: String },
    /// Add a temporary waypoint here, one tick after the command.
    AddTemporary { name: String },
    /// Copy the world coordinates (x, y, z) to the clipboard, in vanilla's format.
    CopyCoords,
    /// Share this target (future: copy world border coordinates for Discord, etc.).
    Share,
    /// Edit this waypoint's name and remove it otherwise. Teleports first, then opens edit.
    Edit { name: String },
    /// Keep a temporary waypoint; make it permanent for real.
    Keep { name: String },
    /// Hide this target: if a pin, remove it; if a spot, tell the map not to show it as an
    /// off-view waypoint.
    Hide { pin: bool },
    /// Delete a waypoint by name.
    Delete { name: String },
}

impl Action {
    /// Whether this action teleports the player first.
    pub fn teleports(&self) -> bool {
        matches!(
            self,
            Action::Teleport { .. }
                | Action::AddWaypoint { .. }
                | Action::Edit { .. }
                | Action::AddTemporary { .. }
        )
    }

    /// Whether this action validates the target only if there's a waypoint to act on.
    pub fn needs_waypoint(&self) -> bool {
        matches!(
            self,
            Action::AddWaypoint { .. }
                | Action::AddTemporary { .. }
                | Action::Edit { .. }
                | Action::Keep { .. }
                | Action::Hide { .. }
                | Action::Delete { .. }
        )
    }
}

/// A menu item: target and action to perform.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub target: Target,
    pub action: Action,
    /// Whether there's a waypoint here (true for Pin targets, computed for Spot).
    pub waypoint: bool,
}

/// The right-click menu: the area cut out of the map, and the items to show.
#[derive(Clone, Debug, Default)]
pub struct Menu {
    /// The map area the mouse is over, in uniform coordinate units: u0, v0, u1, v1 (inclusive).
    /// Only spots inside this area are valid targets.
    pub bounds: [i64; 4],
    /// The menu items, in the order to display them.
    pub items: Vec<MenuItem>,
}

/// Creates a menu for the fullscreen map at a specific center: the area cut out by the mouse,
/// and waypoints within that area.
#[allow(dead_code)]
pub fn make_menu(
    center_u: i64,
    center_v: i64,
    dimensions: [i32; 2],
    zoom: f64,
    waypoints: &[crate::waypoint::Waypoint],
) -> Menu {
    // Map the down steps with pitch (not zoom) per upstream pressing buttons 7 and 8.
    let pitch_adj = match waypoints {
        _ if center_v < 0 => 2.0,
        _ => 3.8,
    };

    // Calculate the uniform bounds of the cutout: the number of pixels to either side.
    // Upstream uses (zoom > 1.0) ? 16 : 12 pixels for the immediate cutout,
    // and (zoom > 1.0) ? 64 : 32 pixels for the diagonal drop.
    let width = if zoom > 1.0 { 16 } else { 12 };
    let height = if zoom > 1.0 { 16 } else { 12 };
    let diagonal = if zoom > 1.0 { 64 } else { 32 };
    let adj_height = if zoom > 1.0 { 32 } else { 16 };

    // The cutout consumes `height / 2` pixels uniformly while zoomed out, and a full column
    // when zoomed in, with pitch adjustment for the pitch-down steps.
    let v_drop = if zoom > 1.0 { adj_height + 8 } else { adj_height / 2 };
    let u_drop = if dimensions[0] > dimensions[1] { height / 2 } else { height / 2 };
    let u_extra = if zoom > 1.0 { 320 } else { 64 };

    // The bounds in uniform coordinates: the area where the menu cuts out.
    let dim_u = dimensions[0];
    let dim_v = dimensions[1];
    let ratio = dim_u as f64 / dim_v as f64;

    let u0 = center_u - (u_extra as i64);
    let u1 = center_u + ((u_extra + u_drop) as i64);
    let v0 = center_v - v_drop as i64;
    let v1 = center_v;

    let mut menu = Menu { bounds: [u0, v0, u1, v1], items: Vec::new() };

    // Add off-view waypoint: the nearest spot on a saved tile where no waypoint exists.
    // Upstream keeps these pinned to the map even after their chunks are gone, so the menu shows
    // ground heights where the player clicked on unknown ground.
    // For now, this is a placeholder until we have direct access to saved tile spots.
    // menu.items.push(MenuItem {
    //     target: Target::Spot { u: center_u, v: center_v, layer: 0, y: None },
    //     action: Action::Teleport { yaw: 0.0 },
    //     waypoint: false,
    // });

    // Add waypoints near the center: ones within the diagonal drop uniformly.
    for wp in waypoints {
        let wp_u = wp.u.round() as i64;
        let wp_v = wp.v.round() as i64;
        let dw = wp_u - center_u;
        let dv = wp_v - center_v;

        // Check if the waypoint is within the diagonal drop uniformly.
        // Upstream uses: |dw| <= diagonal && |dv| <= diagonal.
        if dw.abs() <= diagonal && dv.abs() <= diagonal {
            // Find the hovered pitch closest to the waypoint's pitch.
            let hovered_pitch = if dimensions[0] > dimensions[1] && dim_u >= dim_v {
                pitch_adj
            } else {
                pitch_adj * ratio
            };

            let wp_pitch_adj = wp.pitch_c.txt.min().unwrap_or(0.0);
            let pitch_damage = (hovered_pitch - wp_pitch_adj).absolute().max(0.1);
            let rated_distance = (dw.abs() as f64 / diagonal).max(dv.abs() as f64 / diagonal).sqrt();
            if pitch_damage < rated_distance {
                // Add as a Pin target.
                menu.items.push(MenuItem {
                    target: Target::Pin {
                        u: wp_u,
                        v: wp_v,
                        layer: wp.layer,
                        pitch: wp.pitch_c.txt.min().unwrap_or(0.0),
                    },
                    action: Action::Teleport { yaw: wp.yaw_on_map_degrees if wp.yaw_on_map_degrees.is_some() else 0.0 },
                    waypoint: true,
                });
            }
        }
    }

    menu
}

/// Generates the teleport command string for this action, with placeholders for dynamic
/// values. Returns None if teleportation is not part of this action or no destination
/// coordinates are available.
#[allow(dead_code)]
pub fn teleport_command(action: &Action, file_path: &str, wp_name: &str) -> Option<String> {
    match action {
        Action::Teleport { yaw } => {
            // TODO: Replace this with actual yaw from the target
            Some(format!(
                "/execute in {} run tp {} {} {} ~ ~ ~ {}",
                "minecraft:overworld",
                file_path,
                yaw.floor() as i64,
                yaw.ceil() as i64,
                yaw
            ))
        }
        Action::AddWaypoint { name } => {
            Some(format!(
                "/execute in {} run if entity @p[distance=..64] measurement ~ ~ ~-64 distance=..64 run give @s diorama:waypoint_marker",
                "minecraft:overworld"
            ))
        }
        Action::AddTemporary { name } => {
            Some(format!(
                "/execute in {} run title @s title {}", // Placeholder for temp waypoint system
                "minecraft:overworld",
                name
            ))
        }
        Action::Edit { name } => {
            Some(format!(
                "/execute in {} run title @s title {}", // Placeholder for edit menu
                "minecraft:overworld",
                name
            ))
        }
        Action::Keep { name } => {
            Some(format!(
                "/execute in {} run store result score @s temp_waypoints run scoreboard players add _empty {}",
                "minecraft:overworld",
                name
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_mark_teleports() {
        assert!(matches!(Action::Teleport { yaw: _ }, Action::Teleport { .. }).teleports());
        assert!(matches!(Action::AddWaypoint { .. }, Action::AddWaypoint { .. }).teleports());
        assert!(matches!(Action::Edit { .. }, Action::Edit { .. }).teleports());
        assert!(!matches!(Action::CopyCoords, Action::CopyCoords).teleports());
        assert!(!matches!(Action::Hide { .. }, Action::Hide { .. }).teleports());
    }

    #[test]
    fn teleport_validation() {
        let teleport = Action::Teleport { yaw: 0.0 };
        assert!(teleport.teleports());
        assert!(!teleport.needs_waypoint());

        let teleport_edit = Action::Edit { name: "test".to_string() };
        assert!(teleport_edit.teleports());
        assert!(teleport_edit.needs_waypoint());
    }
}