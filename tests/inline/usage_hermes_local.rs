#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `hermes_local` (hermes spec §4.6, tests 42–46): the sqlite3 `-json`
//! stream parse, the cache and its keep-last-good rule, the Nous rate-limit
//! file, and the `hermes:<name>` observation. The sqlite3 here is a shell
//! stub printing captured `sqlite-out/` fixtures, except in the one
//! `needs sqlite3` test, which builds a real WAL db.

use super::*;
use crate::codex_profiles::CodexState;
use crate::testutil::HomeSandbox;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hermes");
/// 2026-09-01T00:00:00Z, the month the `state-v22.sql` rows sit in.
const SEP_1: i64 = 1_788_220_800;
/// Mid-September 2026, in ms.
const SEP_20_MS: u64 = 1_789_862_400_000;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
}

/// A roster of env-mode openrouter profiles, each with a bare home, written
/// directly (no `hermes new`, so nothing is spawned).
pub(crate) fn roster(names: &[&str]) {
    let dir = crate::profile::tollgate_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let mut toml = String::from("schema_version = 1\n");
    for n in names {
        toml.push_str(&format!(
            "[[profiles]]\nname = \"{n}\"\nprovider = \"openrouter\"\nmode = \"account\"\n\
             auth = \"env\"\nkey_env = \"OPENROUTER_API_KEY\"\ncreated_at = \"2026-09-01T00:00:00Z\"\n"
        ));
        let paths = HermesPaths::for_name(n).unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
    }
    std::fs::write(dir.join("hermes-profiles.toml"), toml).unwrap();
}

/// A PATH holding one `sqlite3` stub that prints `out` and exits `code`,
/// recording its argv to `<bin>/sqlite3.argv`.
fn sqlite_stub(sb: &HomeSandbox, out: &str, code: i32) -> std::ffi::OsString {
    let bin = sb.home().join("stub-bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("sqlite3.out"), out).unwrap();
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\037' \"$a\"; done > '{d}/sqlite3.argv'\ncat '{d}/sqlite3.out'\nexit {code}\n",
        d = bin.display()
    );
    let path = bin.join("sqlite3");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.into_os_string()
}

fn ctx_at(codex: &CodexState, now_ms: u64) -> CollectCtx<'_> {
    CollectCtx {
        config: None,
        codex,
        now_ms,
        interval_ms: 300_000,
        guest_mode: false,
        include_disabled: false,
    }
}

fn touch_db(name: &str) {
    let paths = HermesPaths::for_name(name).unwrap();
    std::fs::write(paths.home.join("state.db"), b"").unwrap();
}

/// Test 42: the captured sqlite3 `-json` streams parse into rows, the month's
/// cost is the exact decimal sum of the six-decimal strings, and an empty
/// month (no third array) is an empty reading, not an error.
#[test]
fn hermes_local_parses_sqlite_json_streams_into_decimal_estimate() {
    let reading = parse_sqlite_json(fixture("sqlite-out/v22-september.json").as_bytes()).unwrap();
    assert_eq!(reading.db_schema_version, Some(22));
    assert_eq!(reading.rows.len(), 1);
    let row = &reading.rows[0];
    assert_eq!(row.billing_provider, "openrouter");
    assert_eq!(row.model, "anthropic/claude-sonnet-4.5");
    assert_eq!(
        (row.api_calls, row.input_tokens, row.output_tokens),
        (7, 1340, 234)
    );
    assert_eq!(
        (
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.reasoning_tokens
        ),
        (50, 10, 5)
    );
    // 0.010000 billed + 0.002345 + 0.000001 estimated, summed by sqlite.
    assert_eq!(row.cost_usd.as_str(), "0.012346");
    assert_eq!(reading.anthropic_since_ms, None);

    let two = parse_sqlite_json(fixture("sqlite-out/v22-anthropic-row.json").as_bytes()).unwrap();
    let cache = HermesUsageCache {
        schema_version: CACHE_SCHEMA,
        read_at_ms: 1,
        db_schema_version: two.db_schema_version,
        period_start: rfc3339_z(SEP_1),
        rows: two.rows,
        anthropic_since_ms: two.anthropic_since_ms,
        nous_reset_at: None,
        error: None,
        hermes_version: None,
    };
    // 0.012346 + 0.100001: exact, where f64 would print 0.11234700000000001.
    assert_eq!(cache.total_cost().as_str(), "0.112347");
    assert_eq!(
        cache.anthropic_since_ms,
        Some(1_788_500_000_250),
        "an anthropic-billed row dates the evidence"
    );

    let empty = parse_sqlite_json(fixture("sqlite-out/v22-empty-month.json").as_bytes()).unwrap();
    assert_eq!(empty.db_schema_version, Some(22));
    assert!(empty.rows.is_empty());

    assert_eq!(month_start_secs(SEP_1 + 19 * 86_400 + 3_600), SEP_1);
    assert_eq!(month_start_secs(SEP_1), SEP_1);
    assert_eq!(rfc3339_z(SEP_1), "2026-09-01T00:00:00Z");
    assert!(
        usage_sql(SEP_1).contains(&format!(">= {SEP_1} ")),
        "the month start is the only interpolated value"
    );
}

/// Test 43: a real WAL-mode `state.db`, read `-readonly`: the rows come back
/// and the db file's mtime and bytes are unchanged.
#[test]
#[ignore = "needs sqlite3"]
fn hermes_local_reads_a_wal_db_readonly() {
    let sb = HomeSandbox::new();
    roster(&["or-main"]);
    let paths = HermesPaths::for_name("or-main").unwrap();
    let db = paths.home.join("state.db");
    let path = std::env::var_os("PATH");
    let sqlite = crate::hermes::resolve::which_on(path.as_deref(), "sqlite3").expect("sqlite3");
    let status = Command::new(&sqlite)
        .arg(&db)
        .arg("PRAGMA journal_mode=WAL;")
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new(&sqlite)
        .arg(&db)
        .stdin(std::fs::File::open(format!("{FIXTURES}/state-v22.sql")).unwrap())
        .status()
        .unwrap();
    assert!(status.success());
    let mtime = |p: &Path| p.metadata().unwrap().modified().unwrap();
    let before = (mtime(&db), std::fs::read(&db).unwrap());

    let cache = refresh_with("or-main", SEP_20_MS, path.as_deref(), None).unwrap();
    assert_eq!(cache.error, None, "{cache:?}");
    assert_eq!(cache.db_schema_version, Some(22));
    assert_eq!(cache.total_cost().as_str(), "0.012346");
    assert_eq!(cache.rows.len(), 1, "August's row is outside the month");
    assert_eq!(
        (mtime(&db), std::fs::read(&db).unwrap()),
        before,
        "a read-only read leaves the db alone"
    );
    drop(sb);
}

/// Test 44: no `sqlite3` on PATH, and a db whose schema lacks a column the
/// query reads, are typed `Unavailable` failures with their own sentences;
/// neither is an error to the caller.
#[test]
fn hermes_local_missing_sqlite3_and_unknown_schema_are_typed_unavailable() {
    let sb = HomeSandbox::new();
    roster(&["or-main"]);
    touch_db("or-main");
    let empty_path = sb.home().join("empty-bin");
    std::fs::create_dir_all(&empty_path).unwrap();

    let cache = refresh_with("or-main", SEP_20_MS, Some(empty_path.as_os_str()), None).unwrap();
    assert_eq!(cache.error, Some(CacheError::Sqlite3Missing));
    assert_eq!(cache.read_at_ms, 0);
    let codex = CodexState::default();
    let state = HermesState::load().unwrap();
    let obs = observe(
        &state.profiles()[0],
        Some(&cache),
        &ctx_at(&codex, SEP_20_MS),
    );
    let failure = obs.failure.expect("a typed failure");
    assert_eq!(failure.kind, FailureKind::Unavailable);
    assert_eq!(
        failure.message,
        "install sqlite3 to read Hermes' local usage"
    );
    assert!(obs.estimate.is_none());
    assert_eq!(
        serde_json::to_value(&cache).unwrap()["error"],
        "sqlite3_missing"
    );

    // The missing-column db: sqlite3 prints Q1 and Q2, then fails Q3.
    let path = sqlite_stub(&sb, &fixture("sqlite-out/v21-missing-column.json"), 1);
    let cache = refresh_with("or-main", SEP_20_MS, Some(&path), None).unwrap();
    assert_eq!(cache.error, Some(CacheError::SchemaUnknown));
    let obs = observe(
        &state.profiles()[0],
        Some(&cache),
        &ctx_at(&codex, SEP_20_MS),
    );
    assert_eq!(
        obs.failure.unwrap().message,
        "Hermes state.db schema not recognised"
    );

    // Garbage output from a failing sqlite3 is unreadable, not a schema verdict.
    let path = sqlite_stub(&sb, "Error: database is locked\n", 1);
    let cache = refresh_with("or-main", SEP_20_MS, Some(&path), None).unwrap();
    assert_eq!(cache.error, Some(CacheError::DbUnreadable));

    // The stub saw the read-only JSON flags and the db path, never stdin.
    let argv = std::fs::read_to_string(sb.home().join("stub-bin/sqlite3.argv")).unwrap();
    let argv: Vec<&str> = argv.split('\u{1f}').collect();
    assert_eq!(&argv[..4], ["-readonly", "-json", "-cmd", ".timeout 2000"]);
    assert!(argv[4].ends_with("hermes-home/state.db"), "{argv:?}");
}

/// A failed read keeps the last good rows (for 7 days) and records why; a
/// home Hermes never used reads as an empty month, not a failure; and last
/// month's rows are no estimate for this one.
#[test]
fn hermes_local_keeps_the_last_good_reading_and_drops_last_months_estimate() {
    let sb = HomeSandbox::new();
    roster(&["or-main"]);
    let codex = CodexState::default();
    let state = HermesState::load().unwrap();
    let profile = &state.profiles()[0];

    let path = sqlite_stub(&sb, "", 0);
    let cache = refresh_with("or-main", SEP_20_MS, Some(&path), None).unwrap();
    assert_eq!(cache.error, None, "no state.db yet: an empty month");
    assert!(cache.rows.is_empty());
    assert!(
        !sb.home().join("stub-bin/sqlite3.argv").exists(),
        "sqlite3 is not run on a missing db"
    );

    touch_db("or-main");
    let path = sqlite_stub(&sb, &fixture("sqlite-out/v22-september.json"), 0);
    let good = refresh_with("or-main", SEP_20_MS, Some(&path), Some("0.19.0")).unwrap();
    assert_eq!(good.total_cost().as_str(), "0.012346");
    assert_eq!(load("or-main").unwrap(), good, "the cache is on disk");
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        let p = HermesPaths::for_name("or-main")
            .unwrap()
            .profile
            .join(CACHE_FILE);
        p.metadata().unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode, 0o600);

    let later = SEP_20_MS + 3_600_000;
    let bad = sqlite_stub(&sb, "Error: database is locked\n", 1);
    let kept = refresh_with("or-main", later, Some(&bad), None).unwrap();
    assert_eq!(kept.error, Some(CacheError::DbUnreadable));
    assert_eq!(kept.rows, good.rows, "the last good rows stay");
    assert_eq!(kept.read_at_ms, SEP_20_MS);
    assert_eq!(
        kept.hermes_version.as_deref(),
        Some("0.19.0"),
        "the launch version carries over"
    );
    let obs = observe(profile, Some(&kept), &ctx_at(&codex, later));
    assert_eq!(
        obs.estimate.as_ref().map(|e| e.amount.as_str()),
        Some("0.012346")
    );
    assert_eq!(
        obs.failure.as_ref().map(|f| f.kind),
        Some(FailureKind::Unavailable)
    );

    let week_later = SEP_20_MS + KEEP_LAST_GOOD_MS + 1;
    let dropped = refresh_with("or-main", week_later, Some(&bad), None).unwrap();
    assert!(dropped.rows.is_empty(), "a week-old reading is dropped");

    // October: September's cache is no estimate for this month.
    let oct_2 = (SEP_1 as u64 + 31 * 86_400) * 1000;
    let obs = observe(profile, Some(&good), &ctx_at(&codex, oct_2));
    assert!(obs.estimate.is_none());
}

/// Test 45: a Nous cooldown in `rate_limits/nous.json` maps to
/// `RateLimited` with its reset while it is in the future; a torn file is
/// ignored.
#[test]
fn nous_rate_limit_file_maps_to_rate_limited_and_torn_file_is_ignored() {
    let sb = HomeSandbox::new();
    roster(&["nous-a"]);
    let paths = HermesPaths::for_name("nous-a").unwrap();
    let rl = paths.home.join("rate_limits");
    std::fs::create_dir_all(&rl).unwrap();
    std::fs::copy(
        format!("{FIXTURES}/rate_limits_nous.json"),
        rl.join("nous.json"),
    )
    .unwrap();
    assert_eq!(read_nous_reset(&paths.home), Some(1_789_863_000));

    let path = sqlite_stub(&sb, "", 0);
    let cache = refresh_with("nous-a", SEP_20_MS, Some(&path), None).unwrap();
    assert_eq!(cache.nous_reset_at, Some(1_789_863_000));
    let codex = CodexState::default();
    let state = HermesState::load().unwrap();
    let obs = observe(
        &state.profiles()[0],
        Some(&cache),
        &ctx_at(&codex, SEP_20_MS),
    );
    let failure = obs.failure.expect("rate limited");
    assert_eq!(failure.kind, FailureKind::RateLimited);
    assert_eq!(
        failure.retry_after,
        Some(Timestamp::from_secs(1_789_863_000))
    );
    let after = observe(
        &state.profiles()[0],
        Some(&cache),
        &ctx_at(&codex, 1_789_863_001_000),
    );
    assert!(after.failure.is_none(), "a past reset is no failure");

    std::fs::write(rl.join("nous.json"), "{\"reset_at\": 17898").unwrap();
    assert_eq!(read_nous_reset(&paths.home), None, "a torn file is ignored");
    std::fs::write(rl.join("nous.json"), vec![b' '; 70 * 1024]).unwrap();
    assert_eq!(read_nous_reset(&paths.home), None, "an oversized file too");
}

/// Test 46: `collect` emits one `hermes:<name>` per roster profile with the
/// `hermes_profile` origin, the Hermes source and the native-login kind; the
/// estimate is the cache's month; an unknown db schema is best effort.
#[test]
fn collect_emits_hermes_origin_ids_and_openapi_origin_enum_updated() {
    let sb = HomeSandbox::new();
    roster(&["or-main", "or-side"]);
    touch_db("or-main");
    let path = sqlite_stub(&sb, &fixture("sqlite-out/v22-september.json"), 0);
    refresh_with("or-main", SEP_20_MS, Some(&path), None).unwrap();

    let codex = CodexState::default();
    let ctx = ctx_at(&codex, SEP_20_MS + 1_000);
    let all = crate::usage::collect::collect_with(
        &ctx,
        &crate::usage::collect::CollectOpts::default(),
        crate::usage::collect::MONITOR_SOURCES,
        &[],
    );
    let ids: Vec<&str> = all.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["hermes:or-main", "hermes:or-side"]);
    let main = &all[0];
    let v = serde_json::to_value(main).unwrap();
    assert_eq!(v["origin"], "hermes_profile");
    assert_eq!(v["source"], "hermes");
    assert_eq!(v["auth"], "native_login");
    assert_eq!(v["plan"], "openrouter · account home");
    assert_eq!(v["freshness"]["state"], "fresh");
    assert_eq!(v["estimate"]["amount"], "0.012346");
    assert_eq!(v["estimate"]["currency"], "USD");
    assert_eq!(v["estimate"]["basis"], BASIS);
    assert_eq!(v["estimate"]["period"]["kind"], "monthly");
    assert_eq!(v["estimate"]["period"]["derived"], true);
    assert!(!main.best_effort);
    assert_eq!(
        all[1].freshness,
        super::super::observation::Freshness::NotFetched
    );
    assert_eq!(
        serde_json::to_value(Origin::HermesProfile).unwrap(),
        "hermes_profile"
    );
    assert_eq!(Origin::HermesProfile.id_prefix(), "hermes");

    // A db schema other than 22 is flagged best effort.
    let v21 = fixture("sqlite-out/v22-september.json").replace("\"version\":22", "\"version\":23");
    let path = sqlite_stub(&sb, &v21, 0);
    let cache = refresh_with("or-main", SEP_20_MS, Some(&path), None).unwrap();
    let state = HermesState::load().unwrap();
    assert!(observe(&state.profiles()[0], Some(&cache), &ctx).best_effort);

    // The provider filter finds them by source.
    let hermes_only = crate::usage::collect::collect_with(
        &ctx,
        &crate::usage::collect::CollectOpts {
            provider: Some("hermes".into()),
            ..Default::default()
        },
        crate::usage::collect::MONITOR_SOURCES,
        &[],
    );
    assert_eq!(hermes_only.len(), 2);
}

/// The daemon leg's due set: a fresh cache is not due, a minute-old one is,
/// and a profile with no cache always is.
#[test]
fn due_profiles_waits_a_minute_per_profile() {
    let sb = HomeSandbox::new();
    roster(&["a", "b"]);
    touch_db("a");
    let path = sqlite_stub(&sb, &fixture("sqlite-out/v22-empty-month.json"), 0);
    let now = crate::usage::now_ms();
    refresh_with("a", now, Some(&path), None).unwrap();
    let state = HermesState::load().unwrap();
    assert_eq!(due_profiles(state.profiles(), now + 1_000), ["b"]);
    assert_eq!(
        due_profiles(state.profiles(), now + REFRESH_EVERY_MS),
        ["a", "b"]
    );
}

/// A deleted profile's cache is never written: that would recreate the
/// directory the delete just removed.
#[test]
fn refresh_never_recreates_a_deleted_profile_dir() {
    let sb = HomeSandbox::new();
    roster(&["gone"]);
    let paths = HermesPaths::for_name("gone").unwrap();
    std::fs::remove_dir_all(&paths.profile).unwrap();
    let path = sqlite_stub(&sb, "", 0);
    assert!(refresh_with("gone", SEP_20_MS, Some(&path), None).is_err());
    assert!(!paths.profile.exists());
}
