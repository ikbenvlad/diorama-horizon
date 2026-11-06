// Diorama's settings page: its buttons' labels.
fn diorama_settings(game: &mut crate::game::Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    crate::game::diorama::settings(game, mod_id, buttons);
}
