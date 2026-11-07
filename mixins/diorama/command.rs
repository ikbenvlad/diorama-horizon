// `/diorama ...` typed, or run by the [Add] link on a shared waypoint.
fn diorama_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    crate::game::diorama::command(game, command, ran);
}
