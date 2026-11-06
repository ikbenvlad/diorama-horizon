// The client dropped a level: its map is saved and closed.
fn diorama_level_unload(game: &mut crate::game::Game<'_>, dimension: &str) {
    crate::game::diorama::level_unload(game, dimension);
}
