// Typing into the fields of a waypoint on the fullscreen map.
fn diorama_screen_char(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    c: char,
    canceled: &mut bool,
) {
    crate::game::diorama::screen_char(game, screen, c, canceled);
}
