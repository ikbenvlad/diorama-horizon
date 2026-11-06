// Dragging moves the fullscreen map.
fn diorama_screen_drag(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    delta: [f32; 2],
    canceled: &mut bool,
) {
    crate::game::diorama::screen_drag(game, screen, delta, canceled);
}
