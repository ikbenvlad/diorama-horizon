// A button of Diorama's settings page was clicked.
fn diorama_click(game: &mut crate::game::Game<'_>, mod_id: &str, button: usize) {
    crate::game::diorama::settings_click(game, mod_id, button);
}
