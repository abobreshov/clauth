#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::observation`: the exact-decimal [`Amount`] (parse, canonical form,
//! numeric order, minor units, shifts, rounding, serde), the RFC 3339
//! [`Timestamp`], message sanitising and the serde spellings readers key on.

use super::*;

fn amt(s: &str) -> Amount {
    Amount::parse(s).unwrap_or_else(|| panic!("{s:?} parses"))
}

#[test]
fn amount_parse_canonicalises_without_losing_digits() {
    let cases = [
        ("13.67", "13.67"),
        ("1132.60", "1132.60"),
        ("0.000123456789", "0.000123456789"),
        ("-5.71", "-5.71"),
        ("+7", "7"),
        ("007.50", "7.50"),
        (".5", "0.5"),
        ("-0", "0"),
        ("-0.000", "0.000"),
        ("  42  ", "42"),
        (
            "123456789012345678901234567890.1",
            "123456789012345678901234567890.1",
        ),
    ];
    for (raw, want) in cases {
        assert_eq!(amt(raw).as_str(), want, "{raw:?}");
    }
}

#[test]
fn amount_parse_rejects_anything_but_a_plain_decimal() {
    for raw in [
        "", "-", ".", "5.", "1e3", "1,000", "$5", "5 USD", "nan", "inf", "--1", "1.2.3",
    ] {
        assert!(Amount::parse(raw).is_none(), "{raw:?} must not parse");
    }
}

#[test]
fn amount_from_f64_keeps_the_shortest_round_trip_spelling() {
    assert_eq!(Amount::from_f64(13.67).unwrap().as_str(), "13.67");
    assert_eq!(Amount::from_f64(-0.2).unwrap().as_str(), "-0.2");
    assert_eq!(Amount::from_f64(1e-7).unwrap().as_str(), "0.0000001");
    assert_eq!(Amount::from_f64(-0.0).unwrap().as_str(), "0");
    assert!(Amount::from_f64(f64::NAN).is_none());
    assert!(Amount::from_f64(f64::INFINITY).is_none());
}

#[test]
fn amount_from_minor_and_scaled_down_are_exact_shifts() {
    assert_eq!(Amount::from_minor(-571, 2).as_str(), "-5.71");
    assert_eq!(Amount::from_minor(5, 2).as_str(), "0.05");
    assert_eq!(Amount::from_minor(1_234_567, 6).as_str(), "1.234567");
    assert_eq!(Amount::from_minor(42, 0).as_str(), "42");
    assert_eq!(amt("1234").scaled_down(2).as_str(), "12.34");
    assert_eq!(amt("5").scaled_down(2).as_str(), "0.05");
    assert_eq!(amt("-1.5").scaled_down(2).as_str(), "-0.015");
    assert_eq!(amt("7").scaled_down(0).as_str(), "7");
}

#[test]
fn amount_orders_numerically_including_negatives_and_trailing_zeros() {
    assert_eq!(amt("1.5"), amt("1.50"));
    assert!(amt("0.009") < amt("0.01"));
    assert!(amt("-5.71") < amt("-0.2"));
    assert!(amt("-0.2") < amt("0"));
    assert!(amt("19.999999") < amt("20"));
    assert!(amt("100") > amt("99.999"));
    assert!(amt("-100") < amt("-99.999"));
    let mut v = [amt("3"), amt("-1"), amt("0.5"), amt("-10.25")];
    v.sort();
    let got: Vec<&str> = v.iter().map(Amount::as_str).collect();
    assert_eq!(got, ["-10.25", "-1", "0.5", "3"]);
}

#[test]
fn amount_round_dp_rounds_half_away_from_zero() {
    let cases = [
        ("5.7", 2, "5.70"),
        ("0.005", 2, "0.01"),
        ("0.0049", 2, "0.00"),
        ("9.995", 2, "10.00"),
        ("-9.995", 2, "-10.00"),
        ("-0.004", 2, "0.00"),
        ("99.5", 0, "100"),
        ("12", 2, "12.00"),
    ];
    for (raw, dp, want) in cases {
        assert_eq!(amt(raw).round_dp(dp).as_str(), want, "{raw} @ {dp}");
    }
}

#[test]
fn amount_sign_helpers() {
    assert!(amt("-0.01").is_negative());
    assert!(!amt("0").is_negative());
    assert!(amt("0.000").is_zero());
    assert!(!amt("0.001").is_zero());
    assert_eq!(amt("-3.20").abs().as_str(), "3.20");
    assert_eq!("4.2".parse::<Amount>().unwrap(), amt("4.20"));
    assert!("x".parse::<Amount>().is_err());
}

#[test]
fn amount_serialises_as_a_string_and_reads_strings_or_numbers() {
    assert_eq!(
        serde_json::to_string(&amt("-0.000123")).unwrap(),
        r#""-0.000123""#
    );
    let from_str: Amount = serde_json::from_str(r#""1132.60""#).unwrap();
    assert_eq!(from_str.as_str(), "1132.60");
    let from_num: Amount = serde_json::from_str("13.67").unwrap();
    assert_eq!(from_num.as_str(), "13.67");
    assert!(serde_json::from_str::<Amount>(r#""1e3""#).is_err());
}

#[test]
fn timestamp_round_trips_as_rfc3339() {
    let t = Timestamp(1_790_000_000);
    let json = serde_json::to_string(&t).unwrap();
    assert_eq!(
        json,
        format!("\"{}\"", crate::usage::epoch_secs_to_iso(1_790_000_000))
    );
    assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), t);
    // Any offset parses back to the same instant.
    let z: Timestamp = serde_json::from_str(r#""2026-09-29T10:00:00Z""#).unwrap();
    let plus: Timestamp = serde_json::from_str(r#""2026-09-29T12:00:00+02:00""#).unwrap();
    assert_eq!(z, plus);
    assert!(serde_json::from_str::<Timestamp>(r#""yesterday""#).is_err());
    assert_eq!(Timestamp::from_ms(1_500), Timestamp(1));
    assert_eq!(Timestamp::from_secs(9).secs(), 9);
}

#[test]
fn sanitize_message_redacts_credentials_and_bounds_length() {
    let got = sanitize_message(
        "401 from api: Bearer abc.def.ghi invalid\nkey sk-or-v1-0123456789abcdef0123",
    );
    assert_eq!(
        got,
        "401 from api: Bearer [redacted] invalid key [redacted]"
    );
    let jwt = "token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmnop expired";
    assert_eq!(sanitize_message(jwt), "token [redacted] expired");
    let long_hex = "id 0123456789abcdef0123456789abcdef01 ok";
    assert_eq!(sanitize_message(long_hex), "id [redacted] ok");
    assert_eq!(sanitize_message("balance too low"), "balance too low");
    let long = "word ".repeat(100);
    let cut = sanitize_message(&long);
    assert_eq!(cut.chars().count(), 160);
    assert!(cut.ends_with('…'));
    assert!(
        Failure::new(FailureKind::AuthRequired, "Bearer x")
            .message
            .contains("[redacted]")
    );
}

#[test]
fn serde_spellings_are_the_published_ones() {
    let s = |v: serde_json::Value| v.to_string();
    for (src, want) in [
        (SourceId::AnthropicOauth, "anthropic_oauth"),
        (SourceId::OllamaCloud, "ollama_cloud"),
        (SourceId::OpenRouter, "openrouter"),
        (SourceId::DeepSeek, "deepseek"),
        (SourceId::MiniMax, "minimax"),
        (SourceId::Zai, "zai"),
        (SourceId::UpstreamClauth, "upstream_clauth"),
    ] {
        assert_eq!(serde_json::to_value(src).unwrap(), want);
        assert_eq!(src.as_str(), want, "as_str matches serde");
    }
    assert_eq!(
        s(serde_json::to_value(Freshness::Stale { since: None }).unwrap()),
        r#"{"state":"stale","since":null}"#
    );
    assert_eq!(
        s(serde_json::to_value(Freshness::NotFetched).unwrap()),
        r#"{"state":"not_fetched"}"#
    );
    assert_eq!(
        s(serde_json::to_value(WindowScope::Model {
            models: vec!["opus".into()]
        })
        .unwrap()),
        r#"{"kind":"model","models":["opus"]}"#
    );
    assert_eq!(
        s(serde_json::to_value(ScopeOrigin::MonitoringCredential { bound: true }).unwrap()),
        r#"{"kind":"monitoring_credential","bound":true}"#
    );
    assert_eq!(
        serde_json::to_value(AuthKind::NativeLogin).unwrap(),
        "native_login"
    );
    assert_eq!(
        serde_json::to_value(Origin::CodexProfile).unwrap(),
        "codex_profile"
    );
    assert_eq!(
        serde_json::to_value(FailureKind::QuotaExhausted).unwrap(),
        "quota_exhausted"
    );
}

#[test]
fn every_source_has_a_serde_round_trip_and_a_display_name() {
    let all = [
        SourceId::AnthropicOauth,
        SourceId::Codex,
        SourceId::Ollama,
        SourceId::OllamaCloud,
        SourceId::OpenRouter,
        SourceId::Nous,
        SourceId::Hermes,
        SourceId::DeepSeek,
        SourceId::Zai,
        SourceId::MiniMax,
        SourceId::Alibaba,
        SourceId::Grok,
        SourceId::Antigravity,
        SourceId::Generic,
        SourceId::UpstreamClauth,
    ];
    for src in all {
        let v = serde_json::to_value(src).unwrap();
        assert_eq!(v, src.as_str());
        assert_eq!(serde_json::from_value::<SourceId>(v).unwrap(), src);
        assert!(!src.display_name().is_empty());
    }
}

#[test]
fn ids_are_namespaced_by_origin() {
    assert_eq!(account_id(Origin::Profile, "work"), "claude:work");
    assert_eq!(account_id(Origin::CodexProfile, "laptop"), "codex:laptop");
    assert_eq!(account_id(Origin::Monitor, "or-mgmt"), "monitor:or-mgmt");
    assert_eq!(account_id(Origin::Upstream, "scifoo"), "upstream:scifoo");
}

#[test]
fn a_skeleton_observation_is_not_fetched_and_finds_windows_and_meters_by_id() {
    let mut obs = AccountObservation::new(
        account_id(Origin::Profile, "w"),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "w",
    );
    assert_eq!(obs.freshness, Freshness::NotFetched);
    assert_eq!(obs.provider, "OpenRouter");
    obs.windows
        .push(QuotaWindow::new(WINDOW_SESSION, "5h", WindowScope::Shared));
    obs.money.push(MoneyMeter::new(
        "wallet",
        "Balance",
        MoneyKind::Balance,
        Amount::zero(),
        "usd",
        MoneyScope::Profile,
    ));
    assert!(obs.window(WINDOW_SESSION).is_some());
    assert!(obs.window(WINDOW_MONTH).is_none());
    assert_eq!(
        obs.meter("wallet").unwrap().currency,
        "USD",
        "codes upper-case"
    );
    let back: AccountObservation =
        serde_json::from_value(serde_json::to_value(&obs).unwrap()).unwrap();
    assert_eq!(back, obs, "an observation round-trips through JSON");
}
