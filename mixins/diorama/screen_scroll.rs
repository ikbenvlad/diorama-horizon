// The wheel zooms the fullscreen map about the pointer.
fn diorama_screen_scroll(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    scroll: [f64; 2],
    canceled: &mut bool,
) {
    crate::game::diorama::screen_scroll(game, screen, scroll, canceled);
}
