// The fullscreen map, over its screen.
fn diorama_screen_draw(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
) {
    crate::game::diorama::screen_draw(game, batch, screen);
}
