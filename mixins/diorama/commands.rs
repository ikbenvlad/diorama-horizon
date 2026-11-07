// Diorama's client commands, so chat completes them: `/diorama waypoint add|list`.
fn diorama_commands(commands: &mut Vec<&'static str>) {
    crate::game::diorama::commands(commands);
}
