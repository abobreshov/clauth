//! Self-update is compiled out of this build (plan §4.0): the gate is shut
//! whatever the saved toggle says, and `spawn` never starts a thread.

use super::*;

#[test]
fn updates_are_disabled_whatever_the_saved_toggle_says() {
    assert!(!updates_enabled(true), "saved on still means no update");
    assert!(!updates_enabled(false), "saved off means no update");
}

#[test]
fn spawn_never_starts_an_update_thread() {
    assert!(spawn(true).is_none(), "saved on spawns nothing");
    assert!(spawn(false).is_none(), "saved off spawns nothing");
}

#[test]
fn the_disabled_message_points_at_a_source_reinstall() {
    assert_eq!(
        DISABLED_MESSAGE,
        "self-update is disabled in this build; reinstall from source"
    );
}
