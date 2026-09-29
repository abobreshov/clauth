#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::collect`: the observation set off real on-disk caches under a
//! `HomeSandbox` — claude OAuth and api-key profiles, codex profiles, the
//! freshness arms, disabled-profile filtering, the two hook slices, id de-dup
//! and the `--account` / `--provider` filters. Caches only; nothing here can
//! reach the network.

use super::*;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, ProfileName};
use crate::profile_cache::write_profile_cache;
use crate::testutil::HomeSandbox;
use crate::usage::observation::{MoneyKind, WINDOW_SESSION};
use crate::usage::{PlanInfo, PlanTier, UsageWindow};

const HOUR_MS: u64 = 3_600_000;

fn oauth(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    p
}

fn deepseek(name: &str) -> Profile {
    Profile::new(
        name.to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-test-not-a-real-key".to_string()),
    )
}

fn warm_oauth(name: &str, five_h: f64, fetched_at: Option<u64>) {
    crate::testutil::register_names(&[name]);
    write_profile_cache(
        &ProfileName::from(name),
        USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(20)),
                ..PlanInfo::default()
            }),
            five_hour: Some(UsageWindow {
                utilization: five_h,
                resets_at: None,
            }),
            fetched_at,
            ..Default::default()
        },
    );
}

fn warm_codex(name: &str) {
    write_profile_cache(
        &ProfileName::from(name),
        USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                codex_plan: Some("pro".to_string()),
                ..PlanInfo::default()
            }),
            five_hour: Some(UsageWindow {
                utilization: 12.0,
                resets_at: None,
            }),
            codex_reset_credits: Some(1),
            ..Default::default()
        },
    );
}

fn config(profiles: Vec<Profile>, active: &str) -> AppConfig {
    let mut c = AppConfig {
        state: AppState::default(),
        profiles,
    };
    c.state.active_profile = Some(active.into());
    c
}

fn ctx<'a>(config: &'a AppConfig, codex: &'a CodexState, now_ms: u64) -> CollectCtx<'a> {
    CollectCtx {
        config: Some(config),
        codex,
        now_ms,
        interval_ms: config.state.refresh_interval_ms,
        guest_mode: false,
        include_disabled: false,
    }
}

fn ids(obs: &[AccountObservation]) -> Vec<&str> {
    obs.iter().map(|o| o.id.as_str()).collect()
}

#[test]
fn claude_oauth_profile_projects_from_its_usage_cache() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    warm_oauth("work", 42.0, Some(now - 60_000));
    let config = config(vec![oauth("work")], "work");
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&config, &codex, now),
        &CollectOpts::default(),
        &[],
        &[],
    );

    assert_eq!(ids(&got), ["claude:work"]);
    let o = &got[0];
    assert_eq!(o.source, SourceId::AnthropicOauth);
    assert_eq!(o.auth, AuthKind::Subscription);
    assert_eq!(o.origin, Origin::Profile);
    assert_eq!(o.provider, "Anthropic");
    assert_eq!(o.label, "work");
    assert!(o.active);
    assert_eq!(o.plan.as_deref(), Some("Max 20x"));
    assert_eq!(o.freshness, Freshness::Fresh);
    assert_eq!(o.observed_at, Some(Timestamp::from_ms(now - 60_000)));
    assert_eq!(
        o.checked_at, None,
        "only a live scheduler knows the last attempt"
    );
    assert_eq!(o.window(WINDOW_SESSION).unwrap().used_pct, Some(42.0));
    assert!(o.failure.is_none());
}

#[test]
fn oauth_freshness_covers_stale_undated_and_never_fetched() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    warm_oauth("old", 10.0, Some(now - 48 * HOUR_MS));
    warm_oauth("undated", 10.0, None);
    let config = config(vec![oauth("old"), oauth("undated"), oauth("cold")], "old");
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&config, &codex, now),
        &CollectOpts::default(),
        &[],
        &[],
    );

    assert_eq!(
        got[0].freshness,
        Freshness::Stale {
            since: Some(Timestamp::from_ms(now - 48 * HOUR_MS))
        }
    );
    assert_eq!(got[1].freshness, Freshness::Stale { since: None });
    assert_eq!(got[1].observed_at, None);
    assert_eq!(got[2].freshness, Freshness::NotFetched);
    assert!(got[2].windows.is_empty() && got[2].money.is_empty());
    assert_eq!(
        got[2].plan.as_deref(),
        Some("Max"),
        "the token's subscription hint"
    );
}

#[test]
fn api_key_profile_projects_its_provider_cache() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["ds"]);
    crate::testutil::write_captured_third_party_cache("ds", crate::testutil::DEEPSEEK_CACHE_BYTES);
    let config = config(vec![oauth("work"), deepseek("ds")], "work");
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&config, &codex, crate::usage::now_ms()),
        &CollectOpts::default(),
        &[],
        &[],
    );
    let ds = got.iter().find(|o| o.id == "claude:ds").unwrap();
    assert_eq!(ds.source, SourceId::DeepSeek);
    assert_eq!(ds.auth, AuthKind::ApiKey);
    assert_eq!(ds.provider, "DeepSeek");
    assert!(!ds.active);
    assert_eq!(
        ds.endpoint.as_deref(),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(ds.freshness, Freshness::Fresh);
    assert!(ds.observed_at.is_some());
    let wallet = ds.meter("wallet").unwrap();
    assert_eq!(wallet.kind, MoneyKind::Balance);
    assert_eq!(wallet.amount.as_str(), "31.45");
    assert_eq!(wallet.currency, "CNY");
}

#[test]
fn a_rejected_key_and_a_quarantined_login_surface_as_failures() {
    let _home = HomeSandbox::new();
    let ds = deepseek("ds");
    crate::testutil::register_names(&["ds", "broke"]);
    let fp = crate::usage::profile_credential_fingerprint(&ds).unwrap();
    crate::profile_cache::write_auth_expired(&ds.name, fp);
    let mut config = config(vec![ds, oauth("broke")], "broke");
    config.state.auth_broken.push("broke".into());
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&config, &codex, crate::usage::now_ms()),
        &CollectOpts::default(),
        &[],
        &[],
    );
    let f = got[0].failure.as_ref().unwrap();
    assert_eq!(f.kind, FailureKind::AuthRequired);
    assert_eq!(f.message, "api key rejected");
    assert_eq!(
        got[1].failure.as_ref().unwrap().kind,
        FailureKind::AuthRequired
    );
    assert!(
        got[1]
            .failure
            .as_ref()
            .unwrap()
            .message
            .contains("tollgate login broke")
    );
}

#[test]
fn disabled_profiles_hide_unless_asked_but_the_active_one_always_shows() {
    let _home = HomeSandbox::new();
    let mut off = oauth("off");
    off.disabled = true;
    let mut active_off = oauth("home");
    active_off.disabled = true;
    let config = config(vec![off, active_off], "home");
    let codex = CodexState::default();
    let mut c = ctx(&config, &codex, crate::usage::now_ms());
    assert_eq!(
        ids(&collect_with(&c, &CollectOpts::default(), &[], &[])),
        ["claude:home"]
    );
    c.include_disabled = true;
    let all = collect_with(&c, &CollectOpts::default(), &[], &[]);
    assert_eq!(ids(&all), ["claude:off", "claude:home"]);
    assert!(all.iter().all(|o| o.disabled));
}

#[test]
fn codex_profiles_follow_the_claude_ones_off_their_own_cache() {
    let _home = HomeSandbox::new();
    crate::testutil::write_codex_state("active_profile = \"cx\"\nprofiles = [\"cx\", \"spare\"]\n");
    warm_codex("cx");
    let codex = CodexState::load().unwrap();
    let config = config(vec![oauth("work")], "work");
    let got = collect_with(
        &ctx(&config, &codex, crate::usage::now_ms()),
        &CollectOpts::default(),
        &[],
        &[],
    );
    assert_eq!(ids(&got), ["claude:work", "codex:cx", "codex:spare"]);
    let cx = &got[1];
    assert_eq!(cx.source, SourceId::Codex);
    assert_eq!(cx.origin, Origin::CodexProfile);
    assert_eq!(cx.provider, "OpenAI");
    assert!(cx.active);
    assert_eq!(cx.plan.as_deref(), Some("pro"));
    assert_eq!(cx.banked_resets, Some(1));
    assert_eq!(cx.freshness, Freshness::Fresh);
    assert!(!got[2].active);
    assert_eq!(got[2].freshness, Freshness::NotFetched);
}

fn monitor_hook(_: &CollectCtx<'_>) -> Vec<AccountObservation> {
    let mut o = AccountObservation::new(
        account_id(Origin::Monitor, "or-mgmt"),
        SourceId::OpenRouter,
        AuthKind::ReadOnly,
        Origin::Monitor,
        "or-mgmt",
    );
    o.plan = Some("monitoring account".to_string());
    vec![o]
}

fn upstream_hook(ctx: &CollectCtx<'_>) -> Vec<AccountObservation> {
    // Sees the context it was given, and duplicates an id to prove de-dup.
    let mut dup = AccountObservation::new(
        "claude:work".to_string(),
        SourceId::UpstreamClauth,
        AuthKind::ReadOnly,
        Origin::Upstream,
        "work",
    );
    dup.active = ctx.guest_mode;
    let up = AccountObservation::new(
        account_id(Origin::Upstream, "scifoo"),
        SourceId::UpstreamClauth,
        AuthKind::ReadOnly,
        Origin::Upstream,
        "scifoo",
    );
    vec![dup, up]
}

#[test]
fn hooks_append_in_order_and_a_duplicate_id_keeps_the_first() {
    let _home = HomeSandbox::new();
    let config = config(vec![oauth("work")], "work");
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&config, &codex, crate::usage::now_ms()),
        &CollectOpts::default(),
        &[monitor_hook],
        &[upstream_hook],
    );
    assert_eq!(
        ids(&got),
        ["claude:work", "monitor:or-mgmt", "upstream:scifoo"]
    );
    assert_eq!(
        got[0].source,
        SourceId::AnthropicOauth,
        "the profile's own row wins"
    );
}

/// The shipped slices carry exactly the registered hooks, in output order:
/// the monitors.toml reader, then the upstream clauth view.
#[test]
fn the_shipped_hook_slices_carry_the_registered_hooks() {
    assert_eq!(MONITOR_SOURCES.len(), 1);
    assert!(std::ptr::fn_addr_eq(
        MONITOR_SOURCES[0],
        crate::usage::monitor::monitor_observations as SourceHook
    ));
    assert_eq!(UPSTREAM_SOURCES.len(), 1);
    assert!(std::ptr::fn_addr_eq(
        UPSTREAM_SOURCES[0],
        crate::usage::upstream::upstream_observations as SourceHook
    ));
}

#[test]
fn account_and_provider_filters_narrow_the_set() {
    let _home = HomeSandbox::new();
    let config = config(vec![oauth("work"), deepseek("ds")], "work");
    let codex = CodexState::default();
    let c = ctx(&config, &codex, crate::usage::now_ms());
    let run = |account: Option<&str>, provider: Option<&str>| {
        let opts = CollectOpts {
            include_disabled: false,
            account: account.map(str::to_string),
            provider: provider.map(str::to_string),
        };
        collect_with(&c, &opts, &[monitor_hook], &[])
            .into_iter()
            .map(|o| o.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(Some("ds"), None), ["claude:ds"]);
    assert_eq!(run(Some("claude:work"), None), ["claude:work"]);
    assert_eq!(run(None, Some("openrouter")), ["monitor:or-mgmt"]);
    assert_eq!(run(None, Some("DeepSeek")), ["claude:ds"]);
    assert_eq!(run(None, Some("anthropic_oauth")), ["claude:work"]);
    assert!(run(Some("nope"), None).is_empty());
}

#[test]
fn collect_reads_the_sandboxed_home_and_writes_nothing() {
    let home = HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx"]);
    let before = snapshot(home.home());
    let got = collect(&CollectOpts::default());
    assert_eq!(ids(&got), ["codex:cx"]);
    assert_eq!(snapshot(home.home()), before, "collect must not write");
}

/// Every file under `dir` with its length, sorted.
fn snapshot(dir: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((path, len));
            }
        }
    }
    out.sort();
    out
}
