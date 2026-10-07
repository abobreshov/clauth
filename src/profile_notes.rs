//! Per-account free-form notes: one plain-text `note.txt` per profile dir.
//!
//! The file rides the profile directory, so a rename carries the note and a
//! delete removes it with the account. claude-roster only: the TUI's usage tab
//! picks claude profiles, so no codex-roster gate sits beside `is_configured`.

/// The per-profile file name under the profile dir (`profiles/<name>/note.txt`).
pub(crate) const NOTE_FILE: &str = "note.txt";

use crate::profile::ProfileName;

/// The account's stored note for read-only display. Invalid UTF-8 is shown
/// lossily; the editor uses the strict loader so it cannot overwrite those bytes.
pub(crate) fn load_note(name: &ProfileName) -> Option<String> {
    let bytes = read_note(name).ok().flatten()?;
    Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|t| !t.is_empty())
}

/// The editable note, distinguishing an absent file from read/decode failures.
pub(crate) fn load_note_for_edit(name: &ProfileName) -> std::io::Result<Option<String>> {
    let Some(bytes) = read_note(name)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("note.txt is not valid UTF-8; fix it by hand: {e}"),
        )
    })?;
    Ok(Some(text).filter(|t| !t.is_empty()))
}

fn read_note(name: &ProfileName) -> std::io::Result<Option<Vec<u8>>> {
    let path = crate::profile_cache::profile_cache_path(name, NOTE_FILE)
        .ok_or_else(|| std::io::Error::other("cannot resolve the profile dir"))?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Store the account's note, 0600 via the tree discipline. An empty note
/// removes the file instead — clearing the note returns the tab to its hint.
/// Refused for a name the roster no longer carries, so a save can never
/// re-create a deleted account's directory.
pub(crate) fn save_note(name: &ProfileName, text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        // Clearing an absent note is fine (NotFound); any other failure is the
        // caller's to surface — a lying `note saved` toast is worse than the
        // error.
        return match crate::profile_cache::profile_cache_path(name, NOTE_FILE) {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
            None => Ok(()),
        };
    }
    // delete/rename hold this same lock across directory and roster updates.
    // Keep the membership check and atomic write together, so the writer's
    // parent-directory creation cannot resurrect a removed account.
    crate::lock::with_state_lock(|_| {
        if !crate::profile::is_configured(name)? {
            return Ok(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "account is no longer in the roster",
            )));
        }
        let Some(path) = crate::profile_cache::profile_cache_path(name, NOTE_FILE) else {
            return Ok(Err(std::io::Error::other("cannot resolve the profile dir")));
        };
        Ok(crate::profile::atomic_write_600(&path, text.as_bytes()))
    })
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
#[path = "../tests/inline/profile_notes.rs"]
mod tests;
