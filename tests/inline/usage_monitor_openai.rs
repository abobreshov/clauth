#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::source::{FakeHttp, resolve_target};
use super::*;
use crate::testutil::HomeSandbox;
fn target(home: &std::path::Path, admin: bool) -> MonitorTarget {
    let mut cfg = super::super::config::MonitorConfig::new("openai", MonitorKind::Openai);
    cfg.api_key_env = Some("PLAIN".into());
    if admin {
        cfg.billing_key_env = Some("ADMIN".into());
    }
    resolve_target(&cfg, home, 1782867600, &|n| {
        Some(
            if n == "ADMIN" {
                "ADMIN-CANARY"
            } else {
                "PLAIN-CANARY"
            }
            .into(),
        )
    })
}
fn reply(status: u16, body: &str) -> HttpReply {
    HttpReply {
        status,
        body: body.into(),
        headers: vec![],
        retry_after_secs: None,
    }
}
fn fake(status: u16, body: &str) -> FakeHttp {
    let body = body.to_string();
    FakeHttp {
        send_reply: Box::new(move |_, _| Ok(reply(status, &body))),
        ..FakeHttp::offline()
    }
}
#[test]
fn openai_models_200_is_valid() {
    let home = HomeSandbox::new();
    let reading = OpenaiSource
        .fetch(&target(home.home(), false), &fake(200, "{}"))
        .unwrap();
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Valid);
    assert!(reading.verdict.is_none());
}
#[test]
fn ratelimit_headers_become_rpm_tpm_windows() {
    let _home = HomeSandbox::new();
    let headers = [
        ("x-ratelimit-limit-requests", "100"),
        ("x-ratelimit-remaining-requests", "25"),
        ("x-ratelimit-reset-requests", "6m0s"),
        ("x-ratelimit-limit-tokens", "1000"),
        ("x-ratelimit-remaining-tokens", "900"),
        ("x-ratelimit-reset-tokens", "1.5s"),
    ]
    .into_iter()
    .map(|(a, b)| (a.into(), b.into()))
    .collect::<Vec<_>>();
    let windows = rate_windows(&headers, "openai", 1000);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].used_pct, Some(75.0));
    assert_eq!(windows[1].used_pct, Some(10.0));
    assert_eq!(windows[0].resets_at.unwrap().secs(), 1360);
    assert_eq!(windows[1].resets_at.unwrap().secs(), 1002);
    assert!(windows.iter().all(|w| !w.chain_eligible));
}
#[test]
fn no_headers_means_no_windows() {
    let _home = HomeSandbox::new();
    assert!(rate_windows(&[], "openai", 1000).is_empty());
}
#[test]
fn go_style_reset_durations_parse() {
    let _home = HomeSandbox::new();
    for (raw, want) in [
        ("6m0s", 360.0),
        ("1.5s", 1.5),
        ("20ms", 0.02),
        ("1h2m3s", 3723.0),
    ] {
        assert_eq!(parse_duration(raw), Some(want));
    }
    for raw in ["", "abc", "-1s", "1", "NaNs", "1.2.3s"] {
        assert_eq!(parse_duration(raw), None);
    }
}
#[test]
fn openai_401_is_invalid() {
    let home = HomeSandbox::new();
    let reading = OpenaiSource
        .fetch(
            &target(home.home(), false),
            &fake(401, r#"{"error":{"code":"invalid_api_key"}}"#),
        )
        .unwrap();
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Invalid);
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::AuthRequired);
}
#[test]
fn openai_429_classifies_on_error_code() {
    let home = HomeSandbox::new();
    for (code, state, kind) in [
        (
            "credit_balance_exhausted",
            KeyHealthState::OutOfCredits,
            FailureKind::QuotaExhausted,
        ),
        (
            "project_spend_limit_exceeded",
            KeyHealthState::SpendCapped,
            FailureKind::QuotaExhausted,
        ),
        (
            "organization_spend_limit_exceeded",
            KeyHealthState::SpendCapped,
            FailureKind::QuotaExhausted,
        ),
        (
            "rate_limit",
            KeyHealthState::Unknown,
            FailureKind::RateLimited,
        ),
    ] {
        let reading = OpenaiSource
            .fetch(
                &target(home.home(), false),
                &fake(429, &format!(r#"{{"error":{{"code":"{code}"}}}}"#)),
            )
            .unwrap();
        assert_eq!(reading.key_health.unwrap().state, state);
        assert_eq!(reading.verdict.unwrap().kind, kind);
    }
}
#[test]
fn plain_key_never_reaches_organization_routes() {
    let home = HomeSandbox::new();
    let http = fake(200, "{}");
    OpenaiSource
        .fetch(&target(home.home(), false), &http)
        .unwrap();
    assert_eq!(http.calls().len(), 1);
    assert!(http.calls()[0].contains("/models auth=bearer:PLAIN-CANARY"));
}
#[test]
fn admin_key_reaches_only_costs() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            if req.url.contains("/costs") {
                assert!(matches!(req.auth,Auth::Bearer(key) if key.expose()=="ADMIN-CANARY"));
                Ok(reply(200, r#"{"data":[]}"#))
            } else {
                assert!(matches!(req.auth,Auth::Bearer(key) if key.expose()=="PLAIN-CANARY"));
                Ok(reply(200, "{}"))
            }
        }),
        ..FakeHttp::offline()
    };
    OpenaiSource
        .fetch(&target(home.home(), true), &http)
        .unwrap();
    assert_eq!(http.calls().len(), 2);
}
#[test]
fn costs_sum_month_to_date_per_currency() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(if req.url.contains("/costs") {
                reply(
                    200,
                    include_str!("../fixtures/monitors/openai_costs_p1.json"),
                )
            } else {
                reply(200, "{}")
            })
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource
        .fetch(&target(home.home(), true), &http)
        .unwrap();
    assert_eq!(reading.money.len(), 2);
    let usd = reading.money.iter().find(|m| m.currency == "USD").unwrap();
    assert_eq!(usd.amount.as_str(), "9007199254740993.30");
    assert_eq!(usd.kind, MoneyKind::Spend);
    assert_eq!(
        usd.period.as_ref().unwrap().start.unwrap().secs(),
        1782864000
    );
    assert!(http.calls()[1].contains("start_time=1782864000&bucket_width=1d&limit=31"));
}
#[test]
fn costs_follow_at_most_three_pages() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(if req.url.contains("/costs") {
                reply(
                    200,
                    r#"{"data":[{"results":[{"amount":{"value":1,"currency":"usd"}}]}],"has_more":true,"next_page":"page2"}"#,
                )
            } else {
                reply(200, "{}")
            })
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource
        .fetch(&target(home.home(), true), &http)
        .unwrap();
    assert_eq!(http.calls().len(), 4);
    assert!(reading.money.is_empty());
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::InvalidResponse);
}
#[test]
fn costs_run_at_most_hourly() {
    let home = HomeSandbox::new();
    let mut target = target(home.home(), true);
    target.previous = Some(Reading {
        costs_at: Some(target.now_secs - 3599),
        money: vec![MoneyMeter::new(
            "spend.monthly",
            "spend",
            MoneyKind::Spend,
            Amount::parse("1.23").unwrap(),
            "USD",
            MoneyScope::Organization,
        )],
        ..Reading::default()
    });
    let http = fake(200, "{}");
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(http.calls().len(), 1);
    assert_eq!(reading.money[0].amount.as_str(), "1.23");
    assert_eq!(reading.costs_at, target.previous.unwrap().costs_at);
}
#[test]
fn costs_403_notes_admin_key() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(reply(
                if req.url.contains("/costs") { 403 } else { 200 },
                "{}",
            ))
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource
        .fetch(&target(home.home(), true), &http)
        .unwrap();
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Valid);
    assert!(reading.note.unwrap().contains("admin key"));
}
#[test]
fn exact_cost_aggregation_and_pagination_fail_closed() {
    let _home = HomeSandbox::new();
    assert_eq!(
        sum(
            &Amount::parse("-1.25").unwrap(),
            &Amount::parse("2.5").unwrap()
        )
        .unwrap()
        .as_str(),
        "1.25"
    );
    assert!(
        sum(
            &Amount::parse("999999999999999999999999999999999999999").unwrap(),
            &Amount::zero()
        )
        .is_none()
    );
}
#[test]
fn configured_plain_key_keeps_api_auth_kind_when_missing_from_process() {
    let home = HomeSandbox::new();
    let mut cfg = super::super::config::MonitorConfig::new("openai", MonitorKind::Openai);
    cfg.api_key_env = Some("PLAIN".into());
    cfg.billing_key_env = Some("ADMIN".into());
    let target = resolve_target(&cfg, home.home(), 1000, &|name| {
        (name == "ADMIN").then(|| "ADMIN-CANARY".into())
    });
    assert!(target.api_key.is_none());
    assert_eq!(OpenaiSource.auth_kind(&target), AuthKind::ApiKey);
    cfg.api_key_env = None;
    let target = resolve_target(&cfg, home.home(), 1000, &|_| Some("ADMIN-CANARY".into()));
    assert_eq!(OpenaiSource.auth_kind(&target), AuthKind::ReadOnly);
}
#[test]
fn costs_403_keeps_known_money_and_failed_costs_keep_verdict_until_due() {
    let home = HomeSandbox::new();
    let mut target = target(home.home(), true);
    let money = MoneyMeter::new(
        "spend.monthly",
        "spend",
        MoneyKind::Spend,
        Amount::parse("3.14").unwrap(),
        "USD",
        MoneyScope::Organization,
    );
    target.previous = Some(Reading {
        costs_at: Some(target.now_secs - 3600),
        money: vec![money.clone()],
        ..Reading::default()
    });
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(reply(
                if req.url.contains("/costs") { 403 } else { 200 },
                "{}",
            ))
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(reading.money, [money]);
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(reply(
                if req.url.contains("/costs") { 500 } else { 200 },
                "{}",
            ))
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(
        reading.verdict.as_ref().unwrap().kind,
        FailureKind::Unavailable
    );
    target.previous = Some(reading);
    target.now_secs += 1;
    let http = fake(200, "{}");
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(http.calls().len(), 1);
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::Unavailable);
    assert!(reading.note.unwrap().contains("costs unavailable"));
}
#[test]
fn costs_two_pages_sum_exactly_and_admin_only_makes_no_models_read() {
    let home = HomeSandbox::new();
    let mut cfg = super::super::config::MonitorConfig::new("admin", MonitorKind::Openai);
    cfg.billing_key_env = Some("ADMIN".into());
    let target = resolve_target(&cfg, home.home(), 1782864000, &|_| {
        Some("ADMIN-CANARY".into())
    });
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            assert!(req.url.contains("/costs"));
            Ok(reply(
                200,
                if req.url.contains("page=second") {
                    r#"{"data":[{"results":[{"amount":{"value":0.2,"currency":"usd"}}]}]}"#
                } else {
                    r#"{"data":[{"results":[{"amount":{"value":0.1,"currency":"usd"}}]}],"has_more":true,"next_page":"second"}"#
                },
            ))
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(http.calls().len(), 2);
    assert_eq!(reading.money[0].amount.as_str(), "0.3");
    assert_eq!(OpenaiSource.auth_kind(&target), AuthKind::ReadOnly);
}
#[test]
fn recovered_plain_health_does_not_reuse_cached_plain_rate_limit() {
    let home = HomeSandbox::new();
    let mut target = target(home.home(), true);
    let costs_failure = Failure::new(FailureKind::Unavailable, "OpenAI costs unavailable");
    target.previous=Some(Reading{costs_at:Some(target.now_secs-1),costs_failure:Some(costs_failure),verdict:Some(Failure::new(FailureKind::RateLimited,"plain key rate limited")),note:Some("no balance endpoint exists for OpenAI keys; costs unavailable; keeping the last reading".into()),..Reading::default()});
    let http = fake(200, "{}");
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(http.calls().len(), 1);
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Valid);
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::Unavailable);
    assert_eq!(
        reading.costs_failure.unwrap().kind,
        FailureKind::Unavailable
    );
}
#[test]
fn admin_costs_rate_limit_preserves_retry_timing() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            let mut response = reply(if req.url.contains("/costs") { 429 } else { 200 }, "{}");
            response.retry_after_secs = Some(120);
            Ok(response)
        }),
        ..FakeHttp::offline()
    };
    let target = target(home.home(), true);
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Valid);
    let failure = reading.verdict.unwrap();
    assert_eq!(failure.kind, FailureKind::RateLimited);
    assert_eq!(failure.retry_after.unwrap().secs(), target.now_secs + 120);
    assert_eq!(
        reading.costs_failure.unwrap().kind,
        FailureKind::RateLimited
    );
}
#[test]
fn costs_old_month_and_seven_day_age_are_never_reused() {
    let home = HomeSandbox::new();
    let mut target = target(home.home(), true);
    let mut meter = MoneyMeter::new(
        "spend.monthly",
        "spend",
        MoneyKind::Spend,
        Amount::parse("7").unwrap(),
        "USD",
        MoneyScope::Organization,
    );
    let mut period = Period::of(PeriodKind::Monthly);
    period.start = Some(Timestamp::from_secs(1782864000 - 1));
    meter.period = Some(period);
    for observed in [target.now_secs - 1, target.now_secs - 8 * 24 * 3600] {
        target.previous = Some(Reading {
            costs_at: Some(target.now_secs - 1),
            costs_observed_at: Some(observed),
            money: vec![meter.clone()],
            ..Reading::default()
        });
        let reading = OpenaiSource.fetch(&target, &fake(200, "{}")).unwrap();
        assert!(reading.money.is_empty());
        assert_eq!(reading.costs_observed_at, Some(observed));
    }
    target.previous = Some(Reading {
        costs_at: Some(target.now_secs - 3600),
        costs_observed_at: Some(target.now_secs - 1),
        money: vec![meter],
        ..Reading::default()
    });
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(reply(
                if req.url.contains("/costs") { 403 } else { 200 },
                "{}",
            ))
        }),
        ..FakeHttp::offline()
    };
    assert!(OpenaiSource.fetch(&target, &http).unwrap().money.is_empty());
}
#[test]
fn repeated_costs_failures_do_not_refresh_spend_age() {
    let home = HomeSandbox::new();
    let mut target = target(home.home(), true);
    target.now_secs += 10 * 24 * 3600;
    let observed = target.now_secs - 100;
    let meter = MoneyMeter::new(
        "spend.monthly",
        "spend",
        MoneyKind::Spend,
        Amount::parse("7").unwrap(),
        "USD",
        MoneyScope::Organization,
    );
    target.previous = Some(Reading {
        costs_at: Some(target.now_secs - 3600),
        costs_observed_at: Some(observed),
        money: vec![meter],
        ..Reading::default()
    });
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(reply(
                if req.url.contains("/costs") { 500 } else { 200 },
                "{}",
            ))
        }),
        ..FakeHttp::offline()
    };
    for _ in 0..3 {
        let reading = OpenaiSource.fetch(&target, &http).unwrap();
        assert_eq!(reading.costs_observed_at, Some(observed));
        assert_eq!(reading.money.len(), 1);
        target.previous = Some(reading);
        target.now_secs += 3600;
    }
    target.now_secs = observed + 7 * 24 * 3600 + 1;
    assert!(OpenaiSource.fetch(&target, &http).unwrap().money.is_empty());
}
#[test]
fn configured_missing_plain_key_reports_auth_required_while_admin_costs_work() {
    let home = HomeSandbox::new();
    let mut cfg = super::super::config::MonitorConfig::new("openai", MonitorKind::Openai);
    cfg.api_key_env = Some("PLAIN".into());
    cfg.billing_key_env = Some("ADMIN".into());
    let target = resolve_target(&cfg, home.home(), 1782864000, &|name| {
        (name == "ADMIN").then(|| "ADMIN-CANARY".into())
    });
    let http = fake(
        200,
        r#"{"data":[{"results":[{"amount":{"value":1,"currency":"usd"}}]}]}"#,
    );
    let reading = OpenaiSource.fetch(&target, &http).unwrap();
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Unknown);
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::AuthRequired);
    assert_eq!(reading.money.len(), 1);
    assert_eq!(http.calls().len(), 1);
    assert!(http.calls()[0].contains("/costs"));
}

#[test]
fn costs_follow_an_opaque_base64_cursor_as_encoded_query_data() {
    let home = HomeSandbox::new();
    let http = FakeHttp {
        send_reply: Box::new(|_, req| {
            Ok(if !req.url.contains("/costs") {
                reply(200, "{}")
            } else if req.url.contains("&page=") {
                assert!(req.url.ends_with("&page=next%2Fpage%2B%3D%3D"));
                reply(200, r#"{"data":[],"has_more":false}"#)
            } else {
                reply(
                    200,
                    r#"{"data":[],"has_more":true,"next_page":"next/page+=="}"#,
                )
            })
        }),
        ..FakeHttp::offline()
    };
    let reading = OpenaiSource
        .fetch(&target(home.home(), true), &http)
        .unwrap();
    assert!(reading.verdict.is_none());
    assert_eq!(http.calls().len(), 3);
}
