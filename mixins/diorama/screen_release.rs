// Letting go ends a drag on the fullscreen map.
fn diorama_screen_release(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    crate::game::diorama::screen_release(game, screen, button, canceled);
}
