//! Flood-wait detection for grammers RPC errors.

/// Parses the seconds out of a Telegram RPC error name (grammers surfaces
/// flood-wait as Rpc errors named "FLOOD_WAIT_<seconds>"). Returns
/// Some(seconds) for "FLOOD_WAIT_30"-style names, Some(0) for a bare
/// "FLOOD_WAIT", None for anything else.
// TODO(M2 green): drop the allow once the parameter is read by the real body.
#[allow(unused_variables)]
pub fn parse_flood_wait(error_name: &str) -> Option<u32> {
    todo!()
}
