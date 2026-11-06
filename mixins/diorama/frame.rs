// Once a frame: the world's changes, bakes, and the map image on show.
fn diorama_frame(game: &mut crate::game::Game<'_>, elapsed: std::time::Duration) {
    crate::game::diorama::frame(game, elapsed);
}
