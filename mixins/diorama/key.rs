// Diorama's keys while playing: the map, and the minimap's zoom.
fn diorama_key(game: &mut crate::game::Game<'_>, key: &str, pressed: bool) {
    crate::game::diorama::key(game, key, pressed);
}
