use super::*;
use crate::profile::{AppConfig, AppState};

fn bare_app() -> App {
    App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    })
}

#[test]
fn the_usage_hints_advertise_the_note_key() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile(&"alice".into());
    let with_account = App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    });
    let mut app = with_account;
    app.tab = Tab::Usage;
    let hints = tab_hints(&app);
    assert!(
        hints.contains(&("n", "note")),
        "usage hints must name the note key, got {hints:?}"
    );

    let mut empty = bare_app();
    empty.tab = Tab::Usage;
    let hints = tab_hints(&empty);
    assert!(
        hints.iter().all(|(k, _)| *k != "n"),
        "an empty roster keeps n = new account off the hint bar, got {hints:?}"
    );
}
