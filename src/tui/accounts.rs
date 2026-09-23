//! Overview account groups and the add-account choices.
//!
//! Groups follow the profile's provider. A profile with no endpoint of its own
//! is `claude`; a recognised endpoint uses that provider's name; anything else
//! is `api`. Codex, Grok and agy are separate native logins and are not mixed
//! into this list.

use crate::profile::Profile;
use crate::provider_monitor::types::ProviderKind;
use crate::providers::Provider;

/// Display order: Claude, then each recognised api provider, then a generic
/// endpoint. Unknown names sort after those, alphabetically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountGroup {
    Claude,
    Provider(Provider),
    Api,
}

impl AccountGroup {
    fn of(profile: &Profile) -> Self {
        match profile.provider {
            Some(provider) => Self::Provider(provider),
            None if profile.is_oauth() => Self::Claude,
            None => Self::Api,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Provider(provider) => provider.display_name(),
            Self::Api => "api",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Claude => 0,
            Self::Provider(Provider::DeepSeek) => 1,
            Self::Provider(Provider::Zai) => 2,
            Self::Provider(Provider::Alibaba) => 3,
            Self::Provider(Provider::OpenRouter) => 4,
            Self::Provider(Provider::MiniMax) => 5,
            Self::Api => 6,
        }
    }
}

/// One added account. `Profile` indexes `config.profiles`. `Codex` indexes the
/// codex roster. `Native` indexes `provider_reports` and is only a listed
/// Grok or Antigravity login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RosterSlot {
    Profile(usize),
    Codex(usize),
    Native(usize),
}

/// Listed Grok and Antigravity reports, in file order. A codex monitor target
/// is the codex roster, not a second group. An unlisted target is not an account.
pub(crate) fn listed_native_pairs(
    reports: &[crate::provider_monitor::ProviderReport],
) -> Vec<(usize, &'static str)> {
    reports
        .iter()
        .enumerate()
        .filter_map(|(idx, report)| {
            if !report.listed {
                return None;
            }
            match report.provider {
                ProviderKind::Grok => Some((idx, "grok")),
                ProviderKind::Antigravity => Some((idx, "antigravity")),
                ProviderKind::Codex => None,
            }
        })
        .collect()
}

/// Added accounts in Overview order. `natives` is `(report index, group label)`
/// for listed Grok (`grok`) and Antigravity (`antigravity`) only.
pub(crate) fn added_account_groups(
    profiles: &[Profile],
    codex_count: usize,
    natives: &[(usize, &'static str)],
) -> Vec<(&'static str, Vec<RosterSlot>)> {
    let mut groups: Vec<(&'static str, Vec<RosterSlot>)> = grouped_profiles(profiles)
        .into_iter()
        .map(|(label, indexes)| {
            (
                label,
                indexes.into_iter().map(RosterSlot::Profile).collect(),
            )
        })
        .collect();
    if codex_count > 0 {
        groups.push(("codex", (0..codex_count).map(RosterSlot::Codex).collect()));
    }
    for label in ["grok", "antigravity"] {
        let slots: Vec<_> = natives
            .iter()
            .filter(|(_, name)| *name == label)
            .map(|(idx, _)| RosterSlot::Native(*idx))
            .collect();
        if !slots.is_empty() {
            groups.push((label, slots));
        }
    }
    groups
}

/// Profile indexes grouped for the Overview, config order inside each group.
pub(crate) fn grouped_profiles(profiles: &[Profile]) -> Vec<(&'static str, Vec<usize>)> {
    let mut groups: Vec<(AccountGroup, Vec<usize>)> = Vec::new();
    for (idx, profile) in profiles.iter().enumerate() {
        let group = AccountGroup::of(profile);
        if let Some((_, indexes)) = groups.iter_mut().find(|(existing, _)| *existing == group) {
            indexes.push(idx);
        } else {
            groups.push((group, vec![idx]));
        }
    }
    groups.sort_by_key(|(group, _)| (group.rank(), group.label()));
    groups
        .into_iter()
        .map(|(group, indexes)| (group.label(), indexes))
        .collect()
}

/// What the add-account list can offer. Detected logins come first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddChoice {
    CaptureClaude,
    AdoptCodex,
    /// `entry` is the Grok auth map key (`issuer::account`), never the token.
    /// `None` when the file holds exactly one official login.
    AddGrok {
        entry: Option<String>,
        label: String,
    },
    AddAgy,
    NewClaude,
    NewApi(Provider),
    GenericApi,
}

impl AddChoice {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::CaptureClaude => "Claude — save the login in use".to_string(),
            Self::AdoptCodex => "Codex — adopt the login on this machine".to_string(),
            Self::AddGrok { label, .. } => format!("Grok — add {label}"),
            Self::AddAgy => "Antigravity — add the login on this machine".to_string(),
            Self::NewClaude => "Claude — new account".to_string(),
            Self::NewApi(provider) => format!("{} — new account", provider.display_name()),
            Self::GenericApi => "Other provider — base url and api key".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddAccountForm {
    pub(crate) choices: Vec<AddChoice>,
    pub(crate) cursor: usize,
}

/// Facts gathered once, when the add list opens.
pub(crate) struct AddInputs {
    pub(crate) claude_live: bool,
    pub(crate) codex_ready: bool,
    /// `(auth entry, display label)` for each official Grok login.
    pub(crate) grok: Vec<(String, String)>,
    /// `auth_entry` of each Grok target already in providers.toml.
    pub(crate) grok_entries: Vec<Option<String>>,
    pub(crate) agy_configured: bool,
}

pub(crate) fn build_add_form(inputs: &AddInputs) -> AddAccountForm {
    let mut choices = Vec::new();
    if inputs.claude_live {
        choices.push(AddChoice::CaptureClaude);
    }
    if inputs.codex_ready {
        choices.push(AddChoice::AdoptCodex);
    }
    for (entry, label) in &inputs.grok {
        if grok_already_listed(&inputs.grok_entries, entry, inputs.grok.len()) {
            continue;
        }
        let entry = (inputs.grok.len() > 1).then(|| entry.clone());
        choices.push(AddChoice::AddGrok {
            entry,
            label: label.clone(),
        });
    }
    if !inputs.agy_configured {
        choices.push(AddChoice::AddAgy);
    }
    choices.push(AddChoice::NewClaude);
    for provider in Provider::ALL {
        choices.push(AddChoice::NewApi(provider));
    }
    choices.push(AddChoice::GenericApi);
    AddAccountForm { choices, cursor: 0 }
}

/// A single Grok target with no `auth_entry` already watches the only login.
/// A target whose `auth_entry` equals this login is that login.
fn grok_already_listed(entries: &[Option<String>], entry: &str, login_count: usize) -> bool {
    entries.iter().any(|existing| {
        existing.as_deref() == Some(entry) || (existing.is_none() && login_count == 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    fn named(name: &str, provider: Option<Provider>, base_url: Option<&str>) -> Profile {
        Profile::new(
            name.to_string(),
            base_url.map(str::to_string),
            Some("k".to_string()),
        )
        .tap_provider(provider)
    }

    trait Tap {
        fn tap_provider(self, provider: Option<Provider>) -> Self;
    }

    impl Tap for Profile {
        fn tap_provider(mut self, provider: Option<Provider>) -> Self {
            // `Profile::new` derives the provider from the base url. Tests that
            // want a group without a real url set it after construction.
            if provider.is_some() {
                self.provider = provider;
            }
            self
        }
    }

    #[test]
    fn accounts_group_by_provider_and_keep_config_order_inside_a_group() {
        let profiles = vec![
            named("zeta", None, Some("https://example.test/v1")),
            named("alpha", None, None),
            named(
                "ds",
                Some(Provider::DeepSeek),
                Some("https://api.deepseek.com"),
            ),
            named("beta", None, None),
        ];
        let groups = grouped_profiles(&profiles);
        let labels: Vec<_> = groups.iter().map(|(label, _)| *label).collect();
        assert_eq!(labels, ["claude", "DeepSeek", "api"]);
        assert_eq!(groups[0].1, vec![1, 3]);
        assert_eq!(groups[1].1, vec![2]);
        assert_eq!(groups[2].1, vec![0]);
    }

    #[test]
    fn added_accounts_follow_overview_order_and_skip_empty_native_groups() {
        let profiles = vec![
            named(
                "ds",
                Some(Provider::DeepSeek),
                Some("https://api.deepseek.com"),
            ),
            named("alpha", None, None),
        ];
        let groups = added_account_groups(
            &profiles,
            1,
            &[(4, "antigravity"), (2, "grok"), (9, "codex")],
        );
        let labels: Vec<_> = groups.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            ["claude", "DeepSeek", "codex", "grok", "antigravity"]
        );
        assert_eq!(groups[0].1, vec![RosterSlot::Profile(1)]);
        assert_eq!(groups[2].1, vec![RosterSlot::Codex(0)]);
        assert_eq!(groups[3].1, vec![RosterSlot::Native(2)]);
        assert_eq!(groups[4].1, vec![RosterSlot::Native(4)]);
    }

    #[test]
    fn detected_logins_lead_the_add_list_and_a_listed_grok_login_is_skipped() {
        let form = build_add_form(&AddInputs {
            claude_live: true,
            codex_ready: true,
            grok: vec![
                ("https://auth.x.ai::one".into(), "one".into()),
                ("https://auth.x.ai::two".into(), "two".into()),
            ],
            grok_entries: vec![Some("https://auth.x.ai::one".into())],
            agy_configured: true,
        });
        assert_eq!(
            form.choices[0],
            AddChoice::CaptureClaude,
            "the live Claude login is the first row"
        );
        assert_eq!(form.choices[1], AddChoice::AdoptCodex);
        assert!(form.choices.iter().any(|choice| matches!(
            choice,
            AddChoice::AddGrok { label, .. } if label == "two"
        )));
        assert!(
            !form.choices.iter().any(|choice| matches!(
                choice,
                AddChoice::AddGrok { label, .. } if label == "one"
            )),
            "the Grok login already in providers.toml is not offered again"
        );
        assert!(!form.choices.contains(&AddChoice::AddAgy));
        assert!(form.choices.contains(&AddChoice::NewClaude));
        assert!(
            form.choices
                .contains(&AddChoice::NewApi(Provider::DeepSeek))
        );
    }
}
