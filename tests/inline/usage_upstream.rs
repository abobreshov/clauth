#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::upstream`: upstream clauth's `status.json` (schema 2, fixture) as
//! read-only `upstream:<name>` observations, only in guest mode, reading
//! nothing but that one file.

use super::*;
use crate::codex_profiles::CodexState;
use crate::testutil::HomeSandbox;
use crate::usage::observation::{WINDOW_SESSION, WINDOW_WEEKLY};

const FEED: &str = include_str!("../fixtures/upstream_status.json");

fn now() -> i64 {
    Timestamp::parse("2026-09-29T12:00:00Z").unwrap().secs()
}

fn fresh(at: Option<u64>) -> Freshness {
    match at {
        Some(_) => Freshness::Fresh,
        None => Freshness::NotFetched,
    }
}

fn project() -> Vec<AccountObservation> {
    project_status(FEED.as_bytes(), now(), fresh)
}

#[test]
fn every_valid_profile_projects_read_only_under_the_upstream_namespace() {
    let got = project();
    assert_eq!(
        got.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        [
            "upstream:work",
            "upstream:side",
            "upstream:ds",
            "upstream:gpt"
        ],
        "the nameless and the badly named entries are skipped"
    );
    for o in &got {
        assert_eq!(o.origin, Origin::Upstream);
        assert_eq!(o.auth, AuthKind::ReadOnly);
        assert_eq!(o.source, SourceId::UpstreamClauth);
        assert!(o.label.ends_with(" (clauth)"), "{}", o.label);
    }
}

#[test]
fn the_active_claude_account_carries_its_windows_and_plan() {
    let got = project();
    let work = &got[0];
    assert_eq!(work.label, "work (clauth)");
    assert!(work.active);
    assert_eq!(work.provider, "Anthropic");
    assert_eq!(work.plan.as_deref(), Some("Max 20x"));
    assert_eq!(work.freshness, Freshness::Fresh);
    assert_eq!(work.observed_at, Timestamp::parse("2026-09-29T11:58:00Z"));
    assert!(work.failure.is_none());
    let ids: Vec<&str> = work.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        [WINDOW_SESSION, WINDOW_WEEKLY, "weekly:opus"],
        "30d is dropped"
    );
    let session = &work.windows[0];
    assert_eq!(session.used_pct, Some(42.0));
    assert!(session.chain_eligible);
    assert_eq!(session.resets_at, Timestamp::parse("2026-09-29T15:00:00Z"));
}

#[test]
fn verdicts_and_staleness_carry_over() {
    let got = project();
    let side = &got[1];
    assert_eq!(
        side.failure.as_ref().unwrap().kind,
        FailureKind::AuthRequired
    );
    assert!(
        side.failure
            .as_ref()
            .unwrap()
            .message
            .contains("clauth login side")
    );
    assert!(matches!(
        side.freshness,
        Freshness::Stale { since: Some(_) }
    ));
    assert!(side.windows.is_empty(), "its lapsed 5h window is dropped");

    let ds = &got[2];
    assert_eq!(ds.provider, "DeepSeek");
    assert_eq!(
        ds.endpoint.as_deref(),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(
        ds.failure.as_ref().unwrap().kind,
        FailureKind::QuotaExhausted
    );

    let gpt = &got[3];
    assert_eq!(gpt.provider, "OpenAI");
    assert_eq!(gpt.plan.as_deref(), Some("plus"));
    assert_eq!(gpt.windows.len(), 1);
}

#[test]
fn an_unknown_schema_or_garbage_yields_nothing() {
    let future = FEED.replacen("\"schema\": 2", "\"schema\": 3", 1);
    assert!(project_status(future.as_bytes(), now(), fresh).is_empty());
    let none = FEED.replacen("\"schema\": 2,", "", 1);
    assert!(project_status(none.as_bytes(), now(), fresh).is_empty());
    assert!(project_status(b"not json", now(), fresh).is_empty());
    let v1 = FEED.replacen("\"schema\": 2", "\"schema\": 1", 1);
    assert_eq!(project_status(v1.as_bytes(), now(), fresh).len(), 4);
}

fn ctx(codex: &CodexState, guest_mode: bool) -> CollectCtx<'_> {
    CollectCtx {
        config: None,
        codex,
        now_ms: u64::try_from(now()).unwrap() * 1000,
        interval_ms: 300_000,
        guest_mode,
        include_disabled: false,
    }
}

fn seed_upstream(home: &std::path::Path) {
    let dir = home.join(crate::identity::UPSTREAM_DATA_DIR_NAME);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(UPSTREAM_STATUS_FILE), FEED).unwrap();
}

#[test]
fn the_hook_projects_only_in_guest_mode() {
    let home = HomeSandbox::new();
    seed_upstream(home.home());
    let codex = CodexState::default();
    assert!(upstream_observations(&ctx(&codex, false)).is_empty());
    let got = upstream_observations(&ctx(&codex, true));
    assert_eq!(got.len(), 4);
    assert_eq!(got[0].id, "upstream:work");
}

#[test]
fn the_hook_reads_only_status_json_and_writes_nothing() {
    let home = HomeSandbox::new();
    seed_upstream(home.home());
    let dir = home.home().join(crate::identity::UPSTREAM_DATA_DIR_NAME);
    // Upstream's other files: a reader that opened them would fail on the
    // unreadable one or the directory in the way.
    std::fs::write(dir.join("profiles.toml"), "garbage = [").unwrap();
    std::fs::create_dir_all(dir.join("credentials")).unwrap();
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| (e.path(), e.metadata().unwrap().modified().unwrap()))
        .collect();
    let codex = CodexState::default();
    assert_eq!(upstream_observations(&ctx(&codex, true)).len(), 4);
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| (e.path(), e.metadata().unwrap().modified().unwrap()))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn the_hook_is_empty_without_a_feed() {
    let _home = HomeSandbox::new();
    let codex = CodexState::default();
    assert!(upstream_observations(&ctx(&codex, true)).is_empty());
}

#[test]
fn guest_mode_collect_shows_upstream_accounts_after_tollgates_own() {
    let home = HomeSandbox::new();
    seed_upstream(home.home());
    let codex = CodexState::default();
    let got = crate::usage::collect::collect_with(
        &ctx(&codex, true),
        &crate::usage::collect::CollectOpts {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        crate::usage::collect::MONITOR_SOURCES,
        crate::usage::collect::UPSTREAM_SOURCES,
    );
    assert_eq!(
        got.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["upstream:work", "upstream:side"]
    );
}
