// Keys on the fullscreen map: caves, the slice, back to the player.
fn diorama_screen_key(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    key: &str,
    canceled: &mut bool,
) {
    crate::game::diorama::screen_key(game, screen, key, canceled);
}
