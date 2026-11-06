// Leaving the server: the map is saved and closed.
fn diorama_disconnecting(game: &crate::game::Game<'_>) {
    crate::game::diorama::disconnecting(game);
}
