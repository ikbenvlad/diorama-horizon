// A waypoint shared in chat gets an [Add] link.
fn diorama_chat_received(
    game: &mut crate::game::Game<'_>,
    message: &mut horizon_client::text::Text,
) {
    crate::game::diorama::chat_received(game, message);
}
