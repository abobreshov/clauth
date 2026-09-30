#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The launch guards (spec §4.3), one refusal per test, against hand-built
//! homes and projections. No child process runs here.

use super::*;
use crate::hermes::profiles::{Auth, Mode, Provider};
use crate::hermes::testkit::passing_projection;
use crate::testutil::HomeSandbox;

fn profile(provider: Provider, auth: Auth, mode: Mode) -> HermesProfile {
    HermesProfile {
        name: "or-main".into(),
        provider,
        model: None,
        mode,
        auth,
        key_env: (auth == Auth::Env).then(|| provider.key_env().to_string()),
        key_fingerprint: None,
        created_at: "2026-09-29T12:00:00Z".into(),
    }
}

fn openrouter() -> HermesProfile {
    profile(Provider::Openrouter, Auth::Env, Mode::Account)
}

/// A laid-out profile's paths (home, shared, child home).
fn laid_out(name: &str) -> HermesPaths {
    let paths = HermesPaths::for_name(name).unwrap();
    super::super::home::build_layout(&paths).unwrap();
    paths
}

fn projection(edit: impl FnOnce(&mut serde_json::Value)) -> ProjectionV1 {
    let mut v = passing_projection("openrouter");
    edit(&mut v);
    super::super::projector::parse_projection(serde_json::to_string(&v).unwrap().as_bytes())
        .expect("the edited projection still matches the schema")
}

fn audit(p: &ProjectionV1, managed: Option<&Path>) -> Result<AuditNotes> {
    audit_projection(
        "or-main",
        Path::new("/h"),
        &openrouter(),
        p,
        managed,
        &BTreeSet::new(),
        Path::new("/venv/bin/hermes"),
    )
}

fn refusal_text(r: Result<impl std::fmt::Debug>) -> String {
    let err = r.expect_err("must refuse");
    assert!(
        err.downcast_ref::<Refusal>().is_some(),
        "a guard refuses with the typed Refusal: {err:#}"
    );
    let text = err.to_string();
    assert!(text.starts_with("tollgate: hermes 'or-main': "), "{text}");
    text
}

/// Test 12a: a home whose parent dir is named `profiles` refuses (M-NAME),
/// missing refuses, and a symlinked home refuses.
#[test]
fn guard_parent_named_profiles_refuses() {
    let home = HomeSandbox::new();
    let paths = HermesPaths::for_name("profiles").unwrap();
    // The parent of `profiles/profiles/hermes-home` is `profiles`.
    let text = refusal_text(g1_shape("or-main", &paths));
    assert!(text.ends_with(M_NAME), "{text}");

    let paths = HermesPaths::for_name("or-main").unwrap();
    let text = refusal_text(g1_shape("or-main", &paths));
    assert!(
        text.contains("home missing; delete and recreate the profile"),
        "{text}"
    );

    crate::profile::mkdir_700(&paths.profile).unwrap();
    let real = home.home().join("real-home");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &paths.home).unwrap();
    let text = refusal_text(g1_shape("or-main", &paths));
    assert!(text.contains("is a symlink"), "{text}");

    // A real home with a loose mode passes and is tightened.
    std::fs::remove_file(&paths.home).unwrap();
    std::fs::create_dir(&paths.home).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&paths.home, std::fs::Permissions::from_mode(0o755)).unwrap();
    g1_shape("or-main", &paths).unwrap();
    assert_eq!(
        std::fs::metadata(&paths.home).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// Test 12b: a home resolving under the operator's `~/.hermes` refuses.
#[test]
fn guard_home_under_dot_hermes_refuses() {
    let home = HomeSandbox::new();
    let paths = laid_out("or-main");
    // Make ~/.hermes a link to the tollgate profiles dir, so the home
    // canonicalizes under it.
    std::os::unix::fs::symlink(paths.profile.parent().unwrap(), home.home().join(".hermes"))
        .unwrap();
    let text = refusal_text(g2_containment("or-main", &paths, home.home()));
    assert!(text.contains("Hermes' default root"), "{text}");
}

/// Test 21c: G2 is computed against the CHILD home. A `<child-home>/.hermes`
/// refuses, while the operator's own `~/.hermes` alone does not decide.
#[test]
fn g2_is_computed_against_the_child_home() {
    let home = HomeSandbox::new();
    let paths = laid_out("or-main");
    std::fs::create_dir_all(home.home().join(".hermes/profiles")).unwrap();
    g2_containment("or-main", &paths, home.home()).expect("the operator ~/.hermes does not decide");

    std::fs::create_dir(paths.child_home.join(".hermes")).unwrap();
    let text = refusal_text(g2_containment("or-main", &paths, home.home()));
    assert!(text.contains("child-home/.hermes exists"), "{text}");
}

/// Test 13 (G3): Hermes' own `active_profile` rule.
#[test]
fn guard_active_profile_refuses_non_default_and_allows_empty_or_default() {
    let _home = HomeSandbox::new();
    let paths = laid_out("or-main");
    let file = paths.home.join("active_profile");
    g3_active_profile("or-main", &paths).unwrap();
    for ok in ["", "\n", "default", " default \n"] {
        std::fs::write(&file, ok).unwrap();
        g3_active_profile("or-main", &paths).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
    }
    std::fs::write(&file, "coder\n").unwrap();
    let text = refusal_text(g3_active_profile("or-main", &paths));
    assert!(text.contains("active_profile names 'coder'"), "{text}");
    assert!(text.contains("hermes-home/profiles/coder"), "{text}");
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/dev/null", &file).unwrap();
    refusal_text(g3_active_profile("or-main", &paths));
}

/// Test 14 (G4).
#[test]
fn guard_profiles_subdir_refuses() {
    let _home = HomeSandbox::new();
    let paths = laid_out("or-main");
    g4_profiles("or-main", &paths).unwrap();
    std::fs::write(paths.home.join("profiles"), "").unwrap();
    let text = refusal_text(g4_profiles("or-main", &paths));
    assert!(
        text.contains("/profiles exists; Hermes sub-profiles"),
        "{text}"
    );
}

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// Test 15 (G5): `-p`, `--profile=`, after a subcommand, after `--`.
#[test]
fn argv_scan_refuses_profile_flags_anywhere_including_after_dashdash() {
    for v in [
        &["-p", "x"][..],
        &["--profile=x"],
        &["--profile", "x"],
        &["chat", "-p", "x"],
        &["--", "-p", "x"],
        &["--prof", "x"],
        &["--profi=x"],
        &["-pfoo"],
    ] {
        let text = refusal_text(g5_argv("or-main", "openrouter", &argv(v)));
        assert!(text.contains("selects a Hermes profile"), "{v:?}: {text}");
    }
    g5_argv(
        "or-main",
        "openrouter",
        &argv(&["chat", "-q", "hi", "--resume", "abc"]),
    )
    .unwrap();
}

/// Test 16 (G5): `--provider` in any spelling, and an `anthropic:` model
/// alias, refuse; an OpenRouter slug `anthropic/…` passes.
#[test]
fn argv_scan_refuses_provider_and_anthropic_colon_model_but_allows_openrouter_slug() {
    // argparse's abbreviations spell the same option.
    for v in [
        &["--provider", "nous"][..],
        &["--provider=nous"],
        &["--prov", "anthropic"],
        &["--provi=anthropic"],
        &["chat", "--pr", "x"],
    ] {
        let text = refusal_text(g5_argv("or-main", "openrouter", &argv(v)));
        assert!(
            text.contains("'--provider' is fixed by the profile (openrouter)"),
            "{text}"
        );
    }
    for v in [
        &["-m", "anthropic:claude-opus-4"][..],
        &["--model", "claude:opus"],
        &["--model=claude-code:sonnet"],
        &["--", "-m", "Anthropic:x"],
        &["-manthropic:claude-opus-4"],
        &["-m=claude:opus"],
        &["--mod", "anthropic:x"],
        &["--mo=claude_code:x"],
    ] {
        let text = refusal_text(g5_argv("or-main", "openrouter", &argv(v)));
        assert!(text.contains("-m routes to anthropic"), "{v:?}: {text}");
    }
    for ok in [
        &["-m", "anthropic/claude-sonnet-4.5"][..],
        &["-manthropic/claude-sonnet-4.5"],
        &["--mod", "openai/gpt-5"],
        // Not a prefix of `--provider` or `--model`: left to Hermes.
        &["--prompt", "anthropic:x"],
        &["--max-turns", "3"],
    ] {
        g5_argv("or-main", "openrouter", &argv(ok)).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
    }
}

/// Test 17 (G6).
#[test]
fn guard_hsp_dotenv_present_refuses() {
    let home = HomeSandbox::new();
    let hsp = home.home().join("sp");
    std::fs::create_dir_all(&hsp).unwrap();
    g6_hsp_env("or-main", &hsp).unwrap();
    std::fs::write(hsp.join(".env"), "").unwrap();
    let text = refusal_text(g6_hsp_env("or-main", &hsp));
    assert!(
        text.contains("sp/.env exists; Hermes loads it into every session"),
        "{text}"
    );
}

/// Test 18 (G7).
#[test]
fn guard_op_env_foreign_key_refuses() {
    let ok = projection(|v| {
        v["env_keys"]["op_env"] =
            serde_json::json!([{"key": "OP_SERVICE_ACCOUNT_TOKEN", "nonblank": true}]);
    });
    audit(&ok, None).unwrap();
    let bad = projection(|v| {
        v["env_keys"]["op_env"] = serde_json::json!([
            {"key": "OP_SERVICE_ACCOUNT_TOKEN", "nonblank": true},
            {"key": "OPENAI_API_KEY", "nonblank": true}
        ]);
    });
    let text = refusal_text(audit(&bad, None));
    assert!(
        text.ends_with(
            "/h/.op.env sets OPENAI_API_KEY; only OP_SERVICE_ACCOUNT_TOKEN is allowed there"
        ),
        "{text}"
    );
}

/// Test 19: `HERMES_MANAGED_DIR` decides alone when set (a file means no
/// scope); unset means the default dir.
#[test]
fn managed_dir_env_override_and_non_dir_disable_match_hermes() {
    let home = HomeSandbox::new();
    let default = home.home().join("etc-hermes");
    let custom = home.home().join("custom");
    let file = home.home().join("a-file");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(&file, "").unwrap();

    assert_eq!(
        managed_dir_from(None, &default),
        None,
        "no default dir, no scope"
    );
    std::fs::create_dir_all(&default).unwrap();
    assert_eq!(managed_dir_from(None, &default), Some(default.clone()));
    assert_eq!(
        managed_dir_from(Some("".into()), &default),
        Some(default.clone()),
        "an empty override is unset"
    );
    assert_eq!(
        managed_dir_from(Some(custom.clone().into_os_string()), &default),
        Some(custom)
    );
    assert_eq!(
        managed_dir_from(Some(file.into_os_string()), &default),
        None,
        "an override naming a file disables the scope; it never falls back"
    );

    // The injected default is what the process-level resolver uses.
    let _scope = crate::hermes::testkit::NoManagedScope::at(&home, &default);
    if std::env::var_os("HERMES_MANAGED_DIR").is_none() {
        assert_eq!(managed_dir(), Some(default));
    }
}

/// Test 20 (G8): each key class refuses; a clean managed dir only warns, and
/// a warning never passes a refusal.
#[test]
fn managed_scope_refuses_each_key_class_and_otherwise_only_warns() {
    let dir = Path::new("/etc/hermes");
    for key in [
        "OPENROUTER_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "NOUS_X",
    ] {
        let p = projection(|v| {
            v["env_keys"]["managed"] = serde_json::json!([{"key": key, "nonblank": true}]);
        });
        let text = refusal_text(audit(&p, Some(dir)));
        assert!(
            text.contains(&format!("the managed Hermes scope /etc/hermes sets {key}")),
            "{text}"
        );
    }
    for key in [
        "model",
        "auxiliary",
        "secrets",
        "fallback_model",
        "credential_pool_strategies",
    ] {
        let p = projection(|v| v["managed_config_top_keys"] = serde_json::json!([key]));
        let text = refusal_text(audit(&p, Some(dir)));
        assert!(
            text.contains(&format!("sets {key}, which outranks")),
            "{text}"
        );
    }
    let clean = projection(|v| {
        v["managed_config_top_keys"] = serde_json::json!(["display"]);
        v["env_keys"]["managed"] = serde_json::json!([{"key": "HTTPS_PROXY", "nonblank": true}]);
    });
    let notes = audit(&clean, Some(dir)).unwrap();
    assert_eq!(
        notes.warnings,
        ["tollgate: note — managed Hermes scope /etc/hermes applies to this session"]
    );
    // A managed dir that warns plus an anthropic route elsewhere: still refused.
    let both = projection(|v| {
        v["managed_config_top_keys"] = serde_json::json!(["display"]);
        v["config"]["model_provider"] = serde_json::json!("anthropic");
    });
    refusal_text(audit(&both, Some(dir)));
}

/// Test 21 (G9): a bulk source refuses; onepassword mapping a scrubbed or
/// anthropic name refuses unless it is the profile's own key.
#[test]
fn bulk_secret_source_and_scrubbed_mapped_target_refuse() {
    let bw = projection(|v| {
        v["config"]["secrets"] = serde_json::json!({"bitwarden": {"enabled": true, "targets": []}});
    });
    let text = refusal_text(audit(&bw, None));
    assert!(
        text.contains("secrets source 'bitwarden' is a bulk source"),
        "{text}"
    );

    let disabled = projection(|v| {
        v["config"]["secrets"] =
            serde_json::json!({"bitwarden": {"enabled": false, "targets": []}});
    });
    audit(&disabled, None).unwrap();

    let plugin = projection(|v| {
        v["config"]["secrets"] = serde_json::json!({"vaultish": {"enabled": true, "targets": []}});
    });
    refusal_text(audit(&plugin, None));

    for target in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GH_TOKEN"] {
        let p = projection(|v| {
            v["config"]["secrets"] =
                serde_json::json!({"onepassword": {"enabled": true, "targets": [target]}});
        });
        let text = refusal_text(audit(&p, None));
        assert!(text.contains(&format!("maps {target}")), "{text}");
    }
    let own = projection(|v| {
        v["config"]["secrets"] = serde_json::json!(
            {"onepassword": {"enabled": true, "targets": ["OPENROUTER_API_KEY", "MY_TOOL_TOKEN"]}}
        );
    });
    audit(&own, None).expect("the profile's own key and an unscrubbed name are fine");
}

/// Test 21d (G10a): `auto`, empty and unset refuse, on a pinned task and on an
/// extra task table.
#[test]
fn auto_auxiliary_provider_refuses() {
    for (task, value) in [
        ("vision", serde_json::json!("auto")),
        ("curator", serde_json::json!("")),
        ("monitor", serde_json::Value::Null),
    ] {
        let p = projection(|v| v["config"]["auxiliary"][task]["provider"] = value.clone());
        let text = refusal_text(audit(&p, None));
        assert!(
            text.contains(&format!("auxiliary.{task}.provider is")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "HOME='/child-home' HERMES_HOME='/h' '/venv/bin/hermes' config set auxiliary.{task}.provider openrouter"
            )),
            "{text}"
        );
        assert!(!text.contains("with 'HOME="), "{text}");
    }
    let missing = projection(|v| {
        v["config"]["auxiliary"]
            .as_object_mut()
            .unwrap()
            .remove("goal_judge");
    });
    let text = refusal_text(audit(&missing, None));
    assert!(
        text.contains("auxiliary.goal_judge.provider is unset"),
        "{text}"
    );

    let extra = projection(|v| {
        v["config"]["auxiliary"]["my_task"] =
            serde_json::json!({"provider": "auto", "base_url_host": null});
    });
    let text = refusal_text(audit(&extra, None));
    assert!(
        text.contains("auxiliary.my_task.provider is auto"),
        "{text}"
    );
}

/// Test 22 (G10): one sub-case per config route.
#[test]
fn anthropic_refused_on_every_config_route() {
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, Edit)> = vec![
        (
            "model.provider",
            Box::new(|v| v["config"]["model_provider"] = "anthropic".into()),
        ),
        (
            "model.provider",
            Box::new(|v| v["config"]["model_provider"] = "claude-code".into()),
        ),
        (
            "fallback_providers",
            Box::new(|v| {
                v["config"]["fallback_providers"] = serde_json::json!(["openrouter", "claude"])
            }),
        ),
        (
            "fallback_providers",
            Box::new(|v| v["config"]["fallback_providers"] = serde_json::json!(["anthropic"])),
        ),
        (
            "fallback_model",
            Box::new(|v| v["config"]["fallback_model"] = serde_json::json!(["nous", "anthropic"])),
        ),
        (
            "auxiliary.vision.provider",
            Box::new(|v| v["config"]["auxiliary"]["vision"]["provider"] = "anthropic".into()),
        ),
        (
            "delegation.provider",
            Box::new(|v| v["config"]["delegation"]["provider"] = "claude".into()),
        ),
        (
            "providers.x.base_url",
            Box::new(
                |v| v["config"]["providers"] = serde_json::json!([{"key": "x", "name": "x", "base_url_host": "api.anthropic.com"}]),
            ),
        ),
        (
            "providers.anthropic",
            Box::new(|v| {
                v["config"]["providers"] =
                    serde_json::json!([{"key": "anthropic", "name": null, "base_url_host": null}])
            }),
        ),
        (
            "custom_providers[0]",
            Box::new(|v| {
                v["config"]["custom_providers"] =
                    serde_json::json!([{"name": "mine", "base_url_host": "api.anthropic.com"}])
            }),
        ),
        (
            "auxiliary.vision.base_url",
            Box::new(|v| {
                v["config"]["auxiliary"]["vision"]["base_url_host"] = "API.Anthropic.com".into()
            }),
        ),
    ];
    for (route, edit) in cases {
        let p = projection(|v| edit(v));
        let text = refusal_text(audit(&p, None));
        assert!(
            text.contains(&format!("{route} routes to anthropic")),
            "{route}: {text}"
        );
        assert!(text.ends_with("(refused on every route in v1)"), "{text}");
    }
    // The OpenRouter slug is not a route to anthropic.
    let slug = projection(|v| v["config"]["model"] = "anthropic/claude-sonnet-4.5".into());
    audit(&slug, None).unwrap();
}

fn view(json: &str) -> super::super::pool::PoolAuthView {
    serde_json::from_str(json).unwrap()
}

/// Test 23 (G11), from `auth.json`: `active_provider`, `providers.anthropic`,
/// a `credential_pool.anthropic` holding `claude_code`, and a torn file.
#[test]
fn anthropic_refused_from_auth_json() {
    let home = HomeSandbox::new();
    let h = home.home().join("h");
    std::fs::create_dir_all(&h).unwrap();
    for (json, route) in [
        (
            r#"{"version":1,"active_provider":"anthropic"}"#,
            "auth.json active_provider",
        ),
        (
            r#"{"version":1,"providers":{"anthropic":{"access_token":"x"}}}"#,
            "auth.json providers.anthropic",
        ),
        (
            r#"{"version":1,"credential_pool":{"anthropic":[{"id":"a","source":"claude_code","access_token":"s"}]}}"#,
            "auth.json credential_pool.anthropic",
        ),
    ] {
        let text = refusal_text(g11_auth("or-main", &h, Some(&view(json))));
        assert!(
            text.contains(&format!("{route} routes to anthropic")),
            "{text}"
        );
    }
    g11_auth(
        "or-main",
        &h,
        Some(&view(
            r#"{"version":1,"active_provider":"openrouter","credential_pool":{"openrouter":[]}}"#,
        )),
    )
    .unwrap();
    // Missing passes; torn refuses.
    g11_read_and_check("or-main", &h).unwrap();
    std::fs::write(h.join("auth.json"), r#"{"version":1,"credential_po"#).unwrap();
    let text = refusal_text(g11_read_and_check("or-main", &h));
    assert!(
        text.contains("cannot audit auth.json; retry when Hermes is not writing it"),
        "{text}"
    );
}

/// Test 21f (G11): Hermes' own PKCE store in the home refuses.
#[test]
fn anthropic_oauth_json_in_the_home_refuses() {
    let home = HomeSandbox::new();
    let h = home.home().join("h");
    std::fs::create_dir_all(&h).unwrap();
    g11_auth("or-main", &h, None).unwrap();
    std::fs::write(h.join(".anthropic_oauth.json"), "{}").unwrap();
    let text = refusal_text(g11_auth("or-main", &h, None));
    assert!(
        text.contains(".anthropic_oauth.json routes to anthropic"),
        "{text}"
    );
}

/// Test 24 (G12): the home `.env` with `ANTHROPIC_BASE_URL`, `.op.env`, and
/// the managed `.env`.
#[test]
fn anthropic_refused_from_env_layers() {
    let home_env = projection(|v| {
        v["env_keys"]["home"] =
            serde_json::json!([{"key": "ANTHROPIC_BASE_URL", "nonblank": false}]);
    });
    let text = refusal_text(g12_env("or-main", &home_env));
    assert!(text.contains(".env routes to anthropic"), "{text}");

    let op = projection(|v| {
        v["env_keys"]["op_env"] =
            serde_json::json!([{"key": "ANTHROPIC_API_KEY", "nonblank": true}]);
    });
    let text = refusal_text(g12_env("or-main", &op));
    assert!(text.contains(".op.env routes to anthropic"), "{text}");

    let managed = projection(|v| {
        v["env_keys"]["managed"] =
            serde_json::json!([{"key": "CLAUDE_CODE_OAUTH_TOKEN", "nonblank": true}]);
    });
    let text = refusal_text(g12_env("or-main", &managed));
    assert!(
        text.contains("the managed .env routes to anthropic"),
        "{text}"
    );

    let blank_cc = projection(|v| {
        v["env_keys"]["home"] =
            serde_json::json!([{"key": "CLAUDE_CODE_OAUTH_TOKEN", "nonblank": false}]);
    });
    g12_env("or-main", &blank_cc).unwrap();
}

#[test]
fn g13_binds_one_account_per_account_home() {
    let or = openrouter();
    let text = refusal_text(g13_auth_add("or-main", &or, "nous", None, false));
    assert!(
        text.contains("an account is one provider; this home is openrouter"),
        "{text}"
    );
    let text = refusal_text(g13_auth_add("or-main", &or, "openrouter", None, true));
    assert!(text.contains("account homes hold one account"), "{text}");
    let nous = profile(Provider::Nous, Auth::Oauth, Mode::Account);
    g13_auth_add("or-main", &nous, "nous", None, false).unwrap();
    let pooled = view(r#"{"credential_pool":{"nous":[{"id":"1"}]}}"#);
    refusal_text(g13_auth_add("or-main", &nous, "nous", Some(&pooled), false));
    let pool = profile(Provider::Openrouter, Auth::Pool, Mode::Pool);
    let two = view(r#"{"credential_pool":{"openrouter":[{"id":"1"},{"id":"2"}]}}"#);
    g13_auth_add("or-main", &pool, "openrouter", Some(&two), false).unwrap();
    refusal_text(g13_auth_add("or-main", &pool, "nous", None, false));
    refusal_text(refuse_anthropic_provider("or-main", "claude"));
}

#[test]
fn the_plugin_scan_reads_env_vars_without_executing() {
    let home = HomeSandbox::new();
    let root = home.home().join("plugins/model-providers");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(
        root.join("a/__init__.py"),
        "raise SystemExit('never run')\nx = P(env_vars=(\"A_KEY\", 'A_URL'),)\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b/__init__.py"),
        "env_vars = (\n  \"B_KEY\",\n)\n",
    )
    .unwrap();
    let found = plugin_env_vars(&[root, home.home().join("absent")]);
    assert_eq!(found, ["A_KEY", "A_URL", "B_KEY"].map(String::from).into());
}
