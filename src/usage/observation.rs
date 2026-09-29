//! The provider-agnostic observation model (plan v3.1 §4.1): one
//! [`AccountObservation`] per account tollgate can say anything about — a
//! claude profile, a codex profile, a monitoring-only account, or an account
//! upstream clauth owns — carrying its quota windows and money meters in one
//! serialisable shape.
//!
//! Domain first, presentation second (plan §3.4): every type here serialises on
//! its own and is what `tollgate usage --json` prints, what `status.json`'s
//! `accounts[]` will carry and what the MCP `profiles` tool will return.
//! Presentation (severity, pace, countdowns, money strings) is derived at read
//! time by [`crate::usage::derive`] and is never stored here.
//!
//! Contracts every producer keeps:
//!
//! - **Real data or a typed unavailable state** (plan §3.1). An unknown figure
//!   is `None`, never a fabricated `0`: `used_pct: None` is "not published",
//!   `Freshness::NotFetched` is "never read", a failure is a [`Failure`].
//! - **Money is exact.** Every amount is an [`Amount`]: a signed decimal held as
//!   its canonical decimal STRING (see [`Amount`] for why a string and not
//!   micro-units). Sub-cent precision and negatives (debt) survive the round
//!   trip through JSON unchanged.
//! - **Times are UTC epoch seconds**, serialised as RFC 3339 strings
//!   ([`Timestamp`]).
//! - **Percentages are unclamped.** `used_pct` above 100 is real (debt, a
//!   blocked account) and below 0 is passed through; readers clamp for display.
//! - **Ids are stable** (see [`AccountObservation::id`]) so a reader can key a
//!   row across runs.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The `usage --json` envelope's `schema_version`. Bump on a breaking change to
/// any type in this module; additive fields do not bump it (readers ignore
/// unknown keys).
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The window id of the rolling 5-hour session window (Anthropic `five_hour`,
/// the codex primary slot, a provider's `5h` bar). [`crate::usage::derive::lead_window`]
/// elects it first.
pub(crate) const WINDOW_SESSION: &str = "session";
/// The window id of the account-wide 7-day window.
pub(crate) const WINDOW_WEEKLY: &str = "weekly";
/// Prefix of a per-model weekly window id: `weekly:<model>` (`weekly:opus`).
pub(crate) const WINDOW_WEEKLY_MODEL_PREFIX: &str = "weekly:";
/// The window id of a monthly pool (Ollama's new pricing, Nous credits).
#[allow(dead_code)] // API for the source agents; no caller yet
pub(crate) const WINDOW_MONTH: &str = "month";

/// Length of the session window, in seconds.
pub(crate) const SESSION_WINDOW_SECS: u64 = 5 * 3600;
/// Length of the weekly window, in seconds.
pub(crate) const WEEKLY_WINDOW_SECS: u64 = 7 * 86_400;

// ── Timestamp ──────────────────────────────────────────────────────────────────

/// A UTC instant with one-second resolution, as epoch seconds. Serialises as an
/// RFC 3339 string (`2026-09-29T10:00:00+00:00`, the spelling every other
/// tollgate feed uses via [`crate::usage::epoch_secs_to_iso`]) and parses any
/// RFC 3339 offset back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Timestamp(pub(crate) i64);

impl Timestamp {
    /// From epoch seconds.
    #[allow(dead_code)] // API for the source agents; no caller yet
    pub(crate) fn from_secs(secs: i64) -> Self {
        Self(secs)
    }

    /// From epoch milliseconds, truncating to the second.
    pub(crate) fn from_ms(ms: u64) -> Self {
        Self(i64::try_from(ms / 1000).unwrap_or(i64::MAX))
    }

    /// Parse an RFC 3339 / ISO-8601 stamp (the shape every cache stores).
    /// `None` for anything [`crate::usage::iso_to_epoch_secs`] rejects.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        crate::usage::iso_to_epoch_secs(s).map(Self)
    }

    /// Epoch seconds.
    pub(crate) fn secs(self) -> i64 {
        self.0
    }

    /// The RFC 3339 spelling this serialises as.
    pub(crate) fn to_rfc3339(self) -> String {
        crate::usage::epoch_secs_to_iso(self.0)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Timestamp::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("not an RFC 3339 timestamp: {s}")))
    }
}

// ── Amount ─────────────────────────────────────────────────────────────────────

/// An exact, signed decimal amount of money.
///
/// **Representation: the canonical decimal string** (`"13.67"`, `"-5.71"`,
/// `"0.000123456789"`), not integer micro-units. Providers hand money over as
/// decimal strings (DeepSeek, Nous, Ollama `activity.cost`) or as JSON numbers
/// with more than six fractional digits (OpenRouter per-request spend), and
/// micro-units would round both; a string carries whatever precision arrived.
/// It is also the JSON shape money APIs use, so `usage --json` readers get a
/// string they can hand to their own decimal type.
///
/// Canonical form: optional `-`, the integer digits with no leading zeros
/// (`"0"` for none), then `.` and the fractional digits exactly as given
/// (trailing zeros kept — `"1132.60"` stays `"1132.60"`). A zero is never
/// negative. Comparison ([`Ord`], [`PartialEq`]) is NUMERIC, so `"1.5"` equals
/// `"1.50"`; the string is kept as given for display fidelity.
///
/// Deserialises from a JSON string or number; always serialises as a string.
#[derive(Debug, Clone)]
pub(crate) struct Amount(String);

impl Amount {
    /// Parse a plain decimal: optional sign, digits, optional `.` + digits.
    /// No exponent, no grouping, no currency. Surrounding whitespace is
    /// ignored. `None` for anything else, including `"5."`, `""`, `"nan"`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (neg, body) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        let (int, frac) = match body.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (body, None),
        };
        if !int.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match frac {
            Some(f) if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) => return None,
            None if int.is_empty() => return None,
            _ => {}
        }
        let int = int.trim_start_matches('0');
        let int = if int.is_empty() { "0" } else { int };
        let zero = int == "0" && frac.is_none_or(|f| f.bytes().all(|b| b == b'0'));
        let mut out = String::with_capacity(s.len() + 1);
        if neg && !zero {
            out.push('-');
        }
        out.push_str(int);
        if let Some(f) = frac {
            out.push('.');
            out.push_str(f);
        }
        Some(Self(out))
    }

    /// From an `f64`, through its shortest round-trip decimal spelling (Rust's
    /// `Display` for `f64` never uses an exponent). `None` for NaN / ±inf. Use
    /// only where the source already IS an `f64` (a cache field); parse the raw
    /// string when one exists.
    pub(crate) fn from_f64(v: f64) -> Option<Self> {
        if !v.is_finite() {
            return None;
        }
        Self::parse(&format!("{v}"))
    }

    /// From an integer count of minor units: `from_minor(-571, 2)` is `-5.71`.
    pub(crate) fn from_minor(minor: i64, exponent: u32) -> Self {
        let digits = minor.unsigned_abs().to_string();
        let neg = minor < 0;
        let raw = if exponent == 0 {
            digits
        } else {
            let e = exponent as usize;
            let padded = format!("{digits:0>width$}", width = e + 1);
            let (i, f) = padded.split_at(padded.len() - e);
            format!("{i}.{f}")
        };
        let signed = if neg { format!("-{raw}") } else { raw };
        // Always a valid decimal by construction.
        Self::parse(&signed).unwrap_or_else(Self::zero)
    }

    /// `0`.
    pub(crate) fn zero() -> Self {
        Self("0".to_string())
    }

    /// This amount divided by `10^places`, exactly (a decimal-point shift):
    /// cents → dollars is `scaled_down(2)`.
    pub(crate) fn scaled_down(&self, places: u32) -> Self {
        if places == 0 {
            return self.clone();
        }
        let (neg, int, frac) = self.parts();
        let p = places as usize;
        let int_padded = format!("{int:0>width$}", width = p + 1);
        let (new_int, moved) = int_padded.split_at(int_padded.len() - p);
        let raw = format!(
            "{}{new_int}.{moved}{frac}",
            if neg { "-" } else { "" },
            frac = frac.unwrap_or("")
        );
        Self::parse(&raw).unwrap_or_else(Self::zero)
    }

    /// The canonical decimal string.
    #[allow(dead_code)] // API for the source agents; no caller yet
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Nearest `f64` — for ratios and bars, never for display or storage.
    pub(crate) fn to_f64(&self) -> f64 {
        self.0.parse().unwrap_or(0.0)
    }

    /// Strictly below zero.
    pub(crate) fn is_negative(&self) -> bool {
        self.0.starts_with('-')
    }

    /// Numerically zero.
    #[allow(dead_code)] // API for the source agents; no caller yet
    pub(crate) fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0' || b == b'.')
    }

    /// The absolute value.
    pub(crate) fn abs(&self) -> Self {
        Self(self.0.trim_start_matches('-').to_string())
    }

    /// Rounded to `dp` fractional digits, half away from zero, always showing
    /// exactly `dp` digits (`"5.7"` → `"5.70"`). A negative amount that rounds
    /// to zero comes back as `"0.00"` (canonical zeros carry no sign); callers
    /// that must show the debt read [`Amount::is_negative`] off the original.
    pub(crate) fn round_dp(&self, dp: u32) -> Self {
        let (neg, int, frac) = self.parts();
        let dp = dp as usize;
        let frac = frac.unwrap_or("");
        // All kept digits as one integer string, plus the first dropped digit.
        let mut kept: Vec<u8> = int.bytes().collect();
        let frac_bytes = frac.as_bytes();
        for i in 0..dp {
            kept.push(*frac_bytes.get(i).unwrap_or(&b'0'));
        }
        let round_up = frac_bytes.get(dp).is_some_and(|d| *d >= b'5');
        if round_up {
            let mut i = kept.len();
            loop {
                if i == 0 {
                    kept.insert(0, b'1');
                    break;
                }
                i -= 1;
                if kept[i] == b'9' {
                    kept[i] = b'0';
                } else {
                    kept[i] += 1;
                    break;
                }
            }
        }
        let split = kept.len() - dp;
        let (i, f) = kept.split_at(split);
        let i = String::from_utf8_lossy(i);
        let f = String::from_utf8_lossy(f);
        let raw = if dp == 0 {
            format!("{}{i}", if neg { "-" } else { "" })
        } else {
            format!("{}{i}.{f}", if neg { "-" } else { "" })
        };
        Self::parse(&raw).unwrap_or_else(Self::zero)
    }

    /// `(negative, integer digits, fractional digits)` of the canonical form.
    fn parts(&self) -> (bool, &str, Option<&str>) {
        let (neg, body) = match self.0.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, self.0.as_str()),
        };
        match body.split_once('.') {
            Some((i, f)) => (neg, i, Some(f)),
            None => (neg, body, None),
        }
    }

    /// Compare magnitudes of two canonical amounts.
    fn cmp_abs(a: &Self, b: &Self) -> Ordering {
        let (_, ai, af) = a.parts();
        let (_, bi, bf) = b.parts();
        ai.len()
            .cmp(&bi.len())
            .then_with(|| ai.cmp(bi))
            .then_with(|| {
                let af = af.unwrap_or("").trim_end_matches('0');
                let bf = bf.unwrap_or("").trim_end_matches('0');
                let width = af.len().max(bf.len());
                format!("{af:0<width$}").cmp(&format!("{bf:0<width$}"))
            })
    }
}

impl PartialEq for Amount {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Amount {}

impl PartialOrd for Amount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Amount {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.is_negative(), other.is_negative()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Self::cmp_abs(self, other),
            (true, true) => Self::cmp_abs(other, self),
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Amount {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| anyhow::anyhow!("not a decimal amount: {s:?}"))
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Num(f64),
        }
        match Raw::deserialize(d)? {
            Raw::Str(s) => Amount::parse(&s)
                .ok_or_else(|| serde::de::Error::custom(format!("not a decimal amount: {s:?}"))),
            Raw::Num(n) => {
                Amount::from_f64(n).ok_or_else(|| serde::de::Error::custom("amount is not finite"))
            }
        }
    }
}

// ── The observation ────────────────────────────────────────────────────────────

/// Everything tollgate can say about one account at read time.
///
/// Every field always serialises (an absent `Option` is `null`, an empty list
/// is `[]`), so the JSON key set is stable for readers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AccountObservation {
    /// Stable account id, `<namespace>:<name>`:
    /// `claude:<profile>` (a `profiles.toml` profile of any provider),
    /// `codex:<profile>` (a `codex-profiles.toml` profile),
    /// `monitor:<id>` (a monitoring-only account with no profile — an
    /// unbound management key, an Ollama monitor key),
    /// `upstream:<profile>` (an account upstream clauth owns, read-only),
    /// `hermes:<profile>` (a `hermes-profiles.toml` Hermes home).
    /// Build it with [`account_id`].
    pub(crate) id: String,
    /// Which integration produced the figures.
    pub(crate) source: SourceId,
    /// How the account authenticates.
    pub(crate) auth: AuthKind,
    /// Human name: the profile name, or a monitor's configured label.
    pub(crate) label: String,
    /// Provider display name (`Anthropic`, `OpenRouter`, `OpenAI`), from
    /// [`SourceId::display_name`] unless the producer knows better.
    pub(crate) provider: String,
    /// The plan / tier label (`Max 5x`, `plus`, `pro-legacy`), provider value
    /// first, else a user-configured label; `None` when neither exists.
    pub(crate) plan: Option<String>,
    /// This account is the one the harness is using now (the global active
    /// profile of its roster).
    pub(crate) active: bool,
    /// The operator disabled this profile (hidden from the chain and polling).
    pub(crate) disabled: bool,
    /// Where the account is defined.
    pub(crate) origin: Origin,
    /// The inference endpoint for an api-key account, `None` for a
    /// first-party subscription. Never carries a credential.
    pub(crate) endpoint: Option<String>,
    /// How current the figures are. Separate from [`Self::failure`]: stale
    /// figures stay visible (plan §3.2).
    pub(crate) freshness: Freshness,
    /// The last known reason the account cannot be read or cannot serve.
    pub(crate) failure: Option<Failure>,
    /// Quota windows, lead-first where the source knows an order.
    pub(crate) windows: Vec<QuotaWindow>,
    /// Money meters: balances, spend, caps, budgets.
    pub(crate) money: Vec<MoneyMeter>,
    /// A local cost estimate, only when attribution is exact.
    pub(crate) estimate: Option<LocalEstimate>,
    /// Banked early-reset passes the account can spend (codex
    /// `rate_limit_reset_credits`); `None` when the source has no such thing.
    pub(crate) banked_resets: Option<i64>,
    /// The figures come from a best-effort scanner rather than a typed
    /// integration; readers should say so.
    pub(crate) best_effort: bool,
    /// When the provider produced the figures shown (the fetch that wrote the
    /// cache). `None` when nothing dates them.
    pub(crate) observed_at: Option<Timestamp>,
    /// When tollgate last TRIED to read the account, successful or not. Only a
    /// live scheduler knows this; cache readers leave it `None`.
    pub(crate) checked_at: Option<Timestamp>,
}

impl AccountObservation {
    /// A skeleton with no figures: `NotFetched`, no failure, empty windows and
    /// money. Producers fill the rest.
    pub(crate) fn new(
        id: String,
        source: SourceId,
        auth: AuthKind,
        origin: Origin,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id,
            source,
            auth,
            label: label.into(),
            provider: source.display_name().to_string(),
            plan: None,
            active: false,
            disabled: false,
            origin,
            endpoint: None,
            freshness: Freshness::NotFetched,
            failure: None,
            windows: Vec::new(),
            money: Vec::new(),
            estimate: None,
            banked_resets: None,
            best_effort: false,
            observed_at: None,
            checked_at: None,
        }
    }

    /// The window with id `id`.
    #[allow(dead_code)] // API for the source agents; no caller yet
    pub(crate) fn window(&self, id: &str) -> Option<&QuotaWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The first meter with `meter_id`.
    #[allow(dead_code)] // API for the source agents; no caller yet
    pub(crate) fn meter(&self, meter_id: &str) -> Option<&MoneyMeter> {
        self.money.iter().find(|m| m.meter_id == meter_id)
    }
}

/// The id namespace of an [`AccountObservation::id`].
pub(crate) fn account_id(origin: Origin, name: &str) -> String {
    format!("{}:{name}", origin.id_prefix())
}

/// Which integration an observation's figures came from. Serialises as
/// [`SourceId::as_str`] (`anthropic_oauth`, `ollama_cloud`, `openrouter`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceId {
    /// Anthropic subscription OAuth (`/api/oauth/usage`).
    AnthropicOauth,
    /// OpenAI codex / ChatGPT login (`wham/usage`).
    Codex,
    /// A local Ollama daemon (`127.0.0.1:11434`).
    Ollama,
    /// Ollama Cloud (`ollama.com`).
    OllamaCloud,
    #[serde(rename = "openrouter")]
    OpenRouter,
    /// Nous Research Portal.
    Nous,
    /// Hermes agent local state (`state.db`, `rate_limits/`).
    Hermes,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Zai,
    #[serde(rename = "minimax")]
    MiniMax,
    Alibaba,
    Grok,
    Antigravity,
    /// An unrecognised api-key endpoint read by the best-effort scanner.
    Generic,
    /// Upstream clauth's own caches, read-only.
    UpstreamClauth,
}

impl SourceId {
    /// The snake_case spelling this serialises as — also what
    /// `tollgate usage --provider` matches.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicOauth => "anthropic_oauth",
            Self::Codex => "codex",
            Self::Ollama => "ollama",
            Self::OllamaCloud => "ollama_cloud",
            Self::OpenRouter => "openrouter",
            Self::Nous => "nous",
            Self::Hermes => "hermes",
            Self::DeepSeek => "deepseek",
            Self::Zai => "zai",
            Self::MiniMax => "minimax",
            Self::Alibaba => "alibaba",
            Self::Grok => "grok",
            Self::Antigravity => "antigravity",
            Self::Generic => "generic",
            Self::UpstreamClauth => "upstream_clauth",
        }
    }

    /// Human provider name.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::AnthropicOauth => "Anthropic",
            Self::Codex => "OpenAI",
            Self::Ollama => "Ollama",
            Self::OllamaCloud => "Ollama Cloud",
            Self::OpenRouter => "OpenRouter",
            Self::Nous => "Nous Portal",
            Self::Hermes => "Hermes",
            Self::DeepSeek => "DeepSeek",
            Self::Zai => "Z.ai",
            Self::MiniMax => "MiniMax",
            Self::Alibaba => "Alibaba",
            Self::Grok => "Grok",
            Self::Antigravity => "Antigravity",
            Self::Generic => "generic",
            Self::UpstreamClauth => "clauth",
        }
    }

    /// The source of a `profiles.toml` api-key profile's typed provider.
    pub(crate) fn from_provider(p: Option<crate::providers::Provider>) -> Self {
        use crate::providers::Provider;
        match p {
            Some(Provider::DeepSeek) => Self::DeepSeek,
            Some(Provider::Zai) => Self::Zai,
            Some(Provider::Alibaba) => Self::Alibaba,
            Some(Provider::OpenRouter) => Self::OpenRouter,
            Some(Provider::MiniMax) => Self::MiniMax,
            Some(Provider::OllamaCloud) => Self::OllamaCloud,
            Some(Provider::OllamaDaemon) => Self::Ollama,
            None => Self::Generic,
        }
    }
}

/// How an account authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthKind {
    /// A subscription login tollgate holds (claude OAuth, codex ChatGPT).
    Subscription,
    /// An inference api key.
    ApiKey,
    /// Both: a subscription pair plus an api-key endpoint.
    Hybrid,
    /// A login another program owns (the Ollama daemon, Hermes' pool).
    NativeLogin,
    /// A read-only monitoring credential or another tool's cache.
    ReadOnly,
}

/// Where an account is defined; also its [`AccountObservation::id`] namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    /// `~/.tollgate/profiles.toml` (`claude:`).
    Profile,
    /// `~/.tollgate/codex-profiles.toml` (`codex:`).
    CodexProfile,
    /// A monitoring-only account (`monitor:`).
    Monitor,
    /// Upstream clauth's state, read-only (`upstream:`).
    Upstream,
    /// `~/.tollgate/hermes-profiles.toml`, a Hermes home tollgate launches
    /// (`hermes:`). Read-only here: Hermes owns its own credentials.
    HermesProfile,
}

impl Origin {
    /// The id namespace.
    pub(crate) fn id_prefix(self) -> &'static str {
        match self {
            Self::Profile => "claude",
            Self::CodexProfile => "codex",
            Self::Monitor => "monitor",
            Self::Upstream => "upstream",
            Self::HermesProfile => "hermes",
        }
    }
}

/// How current an observation's figures are. `{"state": "fresh"}`,
/// `{"state": "stale", "since": "…"}`, `{"state": "not_fetched"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Freshness {
    /// Read within the source's staleness threshold.
    Fresh,
    /// Past the threshold (or undatable). `since` is when the shown figures
    /// were read; `None` when nothing dates them.
    Stale { since: Option<Timestamp> },
    /// Nothing has ever been read.
    NotFetched,
}

/// Why an account cannot be read, or cannot serve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Failure {
    pub(crate) kind: FailureKind,
    /// One human sentence, passed through [`sanitize_message`] — never a raw
    /// provider body, never a credential.
    pub(crate) message: String,
    /// When a retry can succeed (a 429's `retry-after`), when known.
    pub(crate) retry_after: Option<Timestamp>,
}

impl Failure {
    /// A failure whose message is sanitised on the way in.
    pub(crate) fn new(kind: FailureKind, message: &str) -> Self {
        Self {
            kind,
            message: sanitize_message(message),
            retry_after: None,
        }
    }
}

/// The failure taxonomy (plan §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKind {
    /// The credential is dead; only a re-login / new key clears it.
    AuthRequired,
    /// A transient 429.
    RateLimited,
    /// A quota or balance is spent — not transient (an Ollama "session usage
    /// limit" 429, a provider's "balance too low", codex `limit_reached`).
    QuotaExhausted,
    /// The source could not be reached.
    Unavailable,
    /// The source answered in a shape tollgate cannot read.
    InvalidResponse,
    /// The usage-only console session lapsed (Alibaba); inference still works.
    ConsoleExpired,
    /// The subscription is canceled / inactive.
    SubscriptionInactive,
}

/// Longest [`Failure::message`] kept, in chars.
const MAX_MESSAGE_CHARS: usize = 160;

/// Make a message safe to publish: control characters dropped, whitespace
/// collapsed, anything that looks like a credential replaced with
/// `[redacted]` (a word after `Bearer`, an `sk-`/`sess-`-style key, any
/// 32+-char run of token alphabet), and the result cut to 160 chars.
pub(crate) fn sanitize_message(raw: &str) -> String {
    let joined = redact_credentials(raw);
    if joined.chars().count() <= MAX_MESSAGE_CHARS {
        return joined;
    }
    let mut cut: String = joined.chars().take(MAX_MESSAGE_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// [`sanitize_message`] without the length cap: control characters dropped,
/// whitespace collapsed, credential-shaped words replaced with `[redacted]`.
/// For text a person reads whole (a config error) that must still never
/// carry a key.
pub(crate) fn redact_credentials(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for word in cleaned.split_whitespace() {
        if tokenish(word) {
            out.push("[redacted]".to_string());
            redact_next = false;
            continue;
        }
        // After a `Bearer`, the next word's first run is the credential; the
        // punctuation around it (`abc"}`) is kept.
        let (word, pending) = redact_embedded(word, redact_next);
        redact_next = pending;
        out.push(word);
    }
    out.join(" ")
}

/// A token-alphabet character: what keys, JWTs and base64/hex runs are made of.
fn token_alphabet(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_.=+/".contains(c)
}

/// `w` with its non-alphanumeric edges trimmed.
fn token_core(w: &str) -> &str {
    w.trim_matches(|c: char| !c.is_ascii_alphanumeric())
}

/// A credential-shaped word: a 32+-char run of token alphabet, or an
/// `sk-` (`sk-or-`, `sk-ant-`, `sk-nous-` …) / `sess-` / JWT (`ey…`) key of 16+.
fn tokenish(w: &str) -> bool {
    let core = token_core(w);
    let lower = core.to_ascii_lowercase();
    (core.len() >= 32 && core.chars().all(token_alphabet))
        || ((lower.starts_with("sk-") || lower.starts_with("sess-") || lower.starts_with("ey"))
            && core.len() >= 16
            && core.chars().all(token_alphabet))
}

/// A credential embedded in punctuation (`{"token":"sk-…"}`, `key=…`,
/// `"Bearer abc"`): each token-alphabet run inside `word` is judged on its
/// own. A run is redacted when it is a prefixed key, or a 32+ run mixing
/// letters and digits (hex, base64; a plain URL path of words is kept), or
/// when it follows a `Bearer` run. Returns the word with those runs masked
/// (the punctuation around them kept) and whether a trailing `Bearer` asks
/// for the next word to be masked too. `after_bearer` says the previous word
/// ended in one.
fn redact_embedded(word: &str, mut after_bearer: bool) -> (String, bool) {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(first) = rest.chars().next() {
        let in_alphabet = token_alphabet(first);
        let end = rest
            .find(|c: char| token_alphabet(c) != in_alphabet)
            .unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        rest = tail;
        if !in_alphabet {
            out.push_str(run);
            continue;
        }
        let core = token_core(run);
        if core.is_empty() {
            out.push_str(run);
            continue;
        }
        let mixed = core.chars().any(|c| c.is_ascii_digit())
            && core.chars().any(|c| c.is_ascii_alphabetic());
        let lower = core.to_ascii_lowercase();
        let prefixed = lower.starts_with("sk-") || lower.starts_with("sess-");
        let credential = after_bearer || (tokenish(core) && (prefixed || mixed));
        let lead = run.len()
            - run
                .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
                .len();
        // A prefixed key glued to its name by `=` (`api_key=sk-…`): `=` is
        // token alphabet (base64 padding), so the run starts with the name.
        let glued = (!credential)
            .then(|| {
                core.match_indices('=').map(|(i, _)| i + 1).find(|&i| {
                    let tail = &core[i..];
                    let lower = tail.to_ascii_lowercase();
                    (lower.starts_with("sk-") || lower.starts_with("sess-")) && tokenish(tail)
                })
            })
            .flatten();
        if credential {
            // The whole run: base64 padding (`=`) is part of the secret.
            out.push_str("[redacted]");
            after_bearer = false;
        } else if let Some(at) = glued {
            out.push_str(&run[..lead + at]);
            out.push_str("[redacted]");
            out.push_str(&run[lead + core.len()..]);
            after_bearer = false;
        } else {
            out.push_str(run);
            after_bearer = core.eq_ignore_ascii_case("bearer");
        }
    }
    (out, after_bearer)
}

// ── Quota windows ──────────────────────────────────────────────────────────────

/// One rolling or calendar quota window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct QuotaWindow {
    /// Stable per source: [`WINDOW_SESSION`], [`WINDOW_WEEKLY`],
    /// `weekly:<model>`, [`WINDOW_MONTH`], or a slug of the provider's label.
    pub(crate) id: String,
    /// Short human label (`5h`, `7d`, `7d opus`, `30d`).
    pub(crate) label: String,
    /// Share consumed, percent, UNCLAMPED; `None` when not published.
    pub(crate) used_pct: Option<f64>,
    /// The provider says (or the figure proves) this window is spent.
    pub(crate) exhausted: bool,
    /// When the window resets; `None` when the source gives no time.
    pub(crate) resets_at: Option<Timestamp>,
    /// Nominal length, for pace; `None` for a calendar / unknown window.
    pub(crate) window_secs: Option<u64>,
    pub(crate) scope: WindowScope,
    /// Whether today's fallback chain judges this window (the 5h / 7d fold).
    pub(crate) chain_eligible: bool,
    /// Absolute amount consumed, in the provider's own unit (tokens, calls),
    /// when published.
    pub(crate) used: Option<f64>,
    /// Absolute ceiling in the same unit, when published.
    pub(crate) limit: Option<f64>,
    /// Per-model request counts inside this window (Ollama); informational.
    pub(crate) breakdown: Vec<ModelCount>,
}

impl QuotaWindow {
    /// A window with only an id, a label and a scope; everything else unknown.
    pub(crate) fn new(id: impl Into<String>, label: impl Into<String>, scope: WindowScope) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            used_pct: None,
            exhausted: false,
            resets_at: None,
            window_secs: None,
            scope,
            chain_eligible: false,
            used: None,
            limit: None,
            breakdown: Vec::new(),
        }
    }
}

/// What a window's counter covers. `{"kind": "shared"}`,
/// `{"kind": "model", "models": ["opus"]}`, …
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WindowScope {
    /// The account's shared pool across models (Anthropic 5h / 7d).
    Shared,
    /// An account-wide counter that is not the shared pool (OpenRouter free
    /// daily requests, a provider's 30d ceiling).
    Account,
    /// Limited to these models.
    Model { models: Vec<String> },
    /// One product surface (e.g. a coding plan).
    Product,
}

/// Requests made to one model inside a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelCount {
    pub(crate) model: String,
    pub(crate) requests: u64,
}

// ── Money ──────────────────────────────────────────────────────────────────────

/// One money figure.
///
/// `amount` semantics depend on `kind`:
/// - `balance`: what is left to spend (negative = debt). `limit` = the pool's
///   size when the source publishes one.
/// - `spend`: what was spent in `period`. `limit` = the cap on that spend.
/// - `limit`: what is LEFT under a cap (`limit_remaining`); `limit` = the cap.
/// - `budget`: a user-configured budget's remaining amount; `limit` = the budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoneyMeter {
    /// Stable per source: `wallet`, `wallet.granted`, `wallet.topped_up`,
    /// `subscription`, `top_up`, `rollover`, `total_usable`, `spend.daily`,
    /// `spend.weekly`, `spend.monthly`, `spend.lifetime`, `spend.extra`,
    /// `key_limit`, `byok.*`, `window.<label>`.
    pub(crate) meter_id: String,
    pub(crate) label: String,
    pub(crate) kind: MoneyKind,
    pub(crate) amount: Amount,
    /// ISO-4217-style code, upper case (`USD`, `CNY`).
    pub(crate) currency: String,
    pub(crate) limit: Option<Amount>,
    pub(crate) scope: MoneyScope,
    /// Provider id of the scope (org / account), for de-dup. `None` never
    /// de-dups.
    pub(crate) scope_id: Option<String>,
    pub(crate) scope_origin: ScopeOrigin,
    /// `false` for a derived total that must never be summed with its parts.
    pub(crate) additive: bool,
    pub(crate) period: Option<Period>,
}

impl MoneyMeter {
    /// A provider-scoped, additive meter with no limit and no period.
    pub(crate) fn new(
        meter_id: impl Into<String>,
        label: impl Into<String>,
        kind: MoneyKind,
        amount: Amount,
        currency: impl Into<String>,
        scope: MoneyScope,
    ) -> Self {
        Self {
            meter_id: meter_id.into(),
            label: label.into(),
            kind,
            amount,
            currency: currency.into().to_ascii_uppercase(),
            limit: None,
            scope,
            scope_id: None,
            scope_origin: ScopeOrigin::Provider,
            additive: true,
            period: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MoneyKind {
    Balance,
    Spend,
    Limit,
    Budget,
}

/// Whose money a meter describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MoneyScope {
    /// This api key only.
    Key,
    /// The account behind this profile, owner not yet resolved.
    Profile,
    Workspace,
    Organization,
}

/// Who vouches for a meter's scope. `{"kind": "provider"}`,
/// `{"kind": "monitoring_credential", "bound": true}`, `{"kind": "user_label"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ScopeOrigin {
    /// The inference credential's own provider response.
    Provider,
    /// A separate monitoring credential; `bound` once proven to be the same
    /// account as the profile's key.
    MonitoringCredential { bound: bool },
    /// Only a user-configured label groups it; never merged.
    UserLabel,
}

/// The period a spend meter covers: `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Period {
    pub(crate) kind: PeriodKind,
    pub(crate) start: Option<Timestamp>,
    pub(crate) end: Option<Timestamp>,
    /// The bounds were derived by tollgate, not published by the provider.
    pub(crate) derived: bool,
}

impl Period {
    /// A period with no known bounds.
    pub(crate) fn of(kind: PeriodKind) -> Self {
        Self {
            kind,
            start: None,
            end: None,
            derived: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeriodKind {
    Daily,
    Weekly,
    Monthly,
    Lifetime,
    Custom,
}

/// A local cost estimate (token ledger × price table), only when every token
/// is attributable to this account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LocalEstimate {
    pub(crate) amount: Amount,
    pub(crate) currency: String,
    pub(crate) period: Option<Period>,
    /// One phrase naming the inputs (`token ledger × ai-pricelog`).
    pub(crate) basis: String,
}

#[cfg(test)]
#[path = "../../tests/inline/usage_observation.rs"]
mod tests;
