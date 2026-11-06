// Pressing on the fullscreen map starts a drag.
fn diorama_screen_press(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    crate::game::diorama::screen_press(game, screen, button, canceled);
}
