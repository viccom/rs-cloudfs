//! Flood-wait detection for grammers RPC errors.

/// Parses the seconds out of a Telegram RPC error name (grammers surfaces
/// flood-wait as Rpc errors named "FLOOD_WAIT_<seconds>"). Returns
/// Some(seconds) for "FLOOD_WAIT_30"-style names, Some(0) for a bare
/// "FLOOD_WAIT", None for anything else.
pub fn parse_flood_wait(error_name: &str) -> Option<u32> {
    if error_name == "FLOOD_WAIT" {
        return Some(0);
    }
    let seconds = error_name.strip_prefix("FLOOD_WAIT_")?.parse().ok()?;
    Some(seconds)
}
