use std::{
    fmt::{self, Display},
    net::IpAddr,
    path::PathBuf,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::HarnessError;

const DEFAULT_TEXT_URL: &str = "http://127.0.0.1:8080/v1";
const DEFAULT_VISION_URL: &str = "http://127.0.0.1:8081/v1";
const DEFAULT_AUDIO_URL: &str = "http://127.0.0.1:8082/v1";
const DEFAULT_D1_URL: &str = "http://127.0.0.1:8083/v1";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    Cpu,
    Gpu,
    Packaged,
}

impl Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Packaged => "packaged",
        };
        value.fmt(formatter)
    }
}

impl FromStr for Profile {
    type Err = HarnessError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Ok(Self::Cpu),
            "gpu" => Ok(Self::Gpu),
            "packaged" => Ok(Self::Packaged),
            other => Err(HarnessError::InvalidConfiguration(format!(
                "LFM3_PROFILE must be cpu, gpu, or packaged; got {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlamaPaths {
    pub server: Option<PathBuf>,
    pub tts: Option<PathBuf>,
    pub cache: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub max_steps: usize,
    pub max_attempts_per_strategy: usize,
    pub max_strategies: usize,
    pub max_total_attempts: usize,
    pub run_timeout_seconds: u64,
    pub max_tokens: u32,
    pub temperature: f64,
    pub shell_timeout_seconds: u64,
    pub context_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_attempts_per_strategy: 3,
            max_strategies: 2,
            max_total_attempts: 6,
            run_timeout_seconds: 900,
            max_tokens: 4096,
            temperature: 0.1,
            shell_timeout_seconds: 120,
            context_chars: 18_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub workspace: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub profile: Profile,
    pub allow_download: bool,
    pub reuse_external_servers: bool,
    pub text_url: Url,
    pub text_model: String,
    pub vision_url: Url,
    pub vision_model: String,
    pub audio_url: Url,
    pub audio_model: String,
    pub d1_url: Url,
    pub d1_model: String,
    pub llama: LlamaPaths,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: None,
            data_dir: default_data_dir(),
            profile: Profile::Cpu,
            allow_download: false,
            reuse_external_servers: false,
            text_url: default_url(DEFAULT_TEXT_URL),
            text_model: "lfm3-1.7b".to_owned(),
            vision_url: default_url(DEFAULT_VISION_URL),
            vision_model: "lfm3-vl-2b".to_owned(),
            audio_url: default_url(DEFAULT_AUDIO_URL),
            audio_model: "lfm3-audio-2b".to_owned(),
            d1_url: default_url(DEFAULT_D1_URL),
            d1_model: "d1-2b".to_owned(),
            llama: LlamaPaths {
                server: None,
                tts: None,
                cache: None,
            },
            limits: Limits::default(),
        }
    }
}

impl Config {
    /// Loads configuration from `LFM3_*` environment variables.
    pub fn load() -> Result<Self, HarnessError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Loads configuration from an explicit key/value source.
    ///
    /// Keeping environment access behind this function makes configuration tests
    /// deterministic and avoids mutating process-global state.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, HarnessError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let defaults = Self::default();
        let profile = lookup("LFM3_PROFILE")
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(defaults.profile);

        Ok(Self {
            workspace: lookup("LFM3_WORKSPACE").map(PathBuf::from),
            data_dir: lookup("LFM3_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or(defaults.data_dir),
            profile,
            allow_download: parse_bool(
                lookup("LFM3_ALLOW_DOWNLOAD"),
                "LFM3_ALLOW_DOWNLOAD",
                defaults.allow_download,
            )?,
            reuse_external_servers: parse_bool(
                lookup("LFM3_REUSE_EXTERNAL_SERVERS"),
                "LFM3_REUSE_EXTERNAL_SERVERS",
                defaults.reuse_external_servers,
            )?,
            text_url: parse_url(lookup("LFM3_TEXT_URL"), "LFM3_TEXT_URL", defaults.text_url)?,
            text_model: lookup("LFM3_TEXT_MODEL").unwrap_or(defaults.text_model),
            vision_url: parse_url(
                lookup("LFM3_VISION_URL"),
                "LFM3_VISION_URL",
                defaults.vision_url,
            )?,
            vision_model: lookup("LFM3_VISION_MODEL").unwrap_or(defaults.vision_model),
            audio_url: parse_url(
                lookup("LFM3_AUDIO_URL"),
                "LFM3_AUDIO_URL",
                defaults.audio_url,
            )?,
            audio_model: lookup("LFM3_AUDIO_MODEL").unwrap_or(defaults.audio_model),
            d1_url: parse_url(lookup("LFM3_D1_URL"), "LFM3_D1_URL", defaults.d1_url)?,
            d1_model: lookup("LFM3_D1_MODEL").unwrap_or(defaults.d1_model),
            llama: LlamaPaths {
                server: lookup("LFM3_LLAMA_SERVER").map(PathBuf::from),
                tts: lookup("LFM3_LLAMA_TTS").map(PathBuf::from),
                cache: lookup("LLAMA_CACHE").map(PathBuf::from),
            },
            limits: Limits {
                max_steps: parse_number(
                    lookup("LFM3_MAX_STEPS"),
                    "LFM3_MAX_STEPS",
                    defaults.limits.max_steps,
                )?,
                max_attempts_per_strategy: parse_number(
                    lookup("LFM3_MAX_ATTEMPTS_PER_STRATEGY"),
                    "LFM3_MAX_ATTEMPTS_PER_STRATEGY",
                    defaults.limits.max_attempts_per_strategy,
                )?,
                max_strategies: parse_number(
                    lookup("LFM3_MAX_STRATEGIES"),
                    "LFM3_MAX_STRATEGIES",
                    defaults.limits.max_strategies,
                )?,
                max_total_attempts: parse_number(
                    lookup("LFM3_MAX_TOTAL_ATTEMPTS"),
                    "LFM3_MAX_TOTAL_ATTEMPTS",
                    defaults.limits.max_total_attempts,
                )?,
                run_timeout_seconds: parse_number(
                    lookup("LFM3_RUN_TIMEOUT_SECONDS"),
                    "LFM3_RUN_TIMEOUT_SECONDS",
                    defaults.limits.run_timeout_seconds,
                )?,
                max_tokens: parse_number(
                    lookup("LFM3_MAX_TOKENS"),
                    "LFM3_MAX_TOKENS",
                    defaults.limits.max_tokens,
                )?,
                temperature: parse_number(
                    lookup("LFM3_TEMPERATURE"),
                    "LFM3_TEMPERATURE",
                    defaults.limits.temperature,
                )?,
                shell_timeout_seconds: parse_number(
                    lookup("LFM3_SHELL_TIMEOUT"),
                    "LFM3_SHELL_TIMEOUT",
                    defaults.limits.shell_timeout_seconds,
                )?,
                context_chars: parse_number(
                    lookup("LFM3_CONTEXT_CHARS"),
                    "LFM3_CONTEXT_CHARS",
                    defaults.limits.context_chars,
                )?,
            },
        })
    }

    /// Validates the limits that apply in every profile.
    pub fn validate(&self) -> Result<(), HarnessError> {
        let limits = &self.limits;
        if !(1..=32).contains(&limits.max_steps) {
            return Err(invalid("LFM3_MAX_STEPS must be between 1 and 32"));
        }
        if !(1..=8).contains(&limits.max_attempts_per_strategy) {
            return Err(invalid(
                "LFM3_MAX_ATTEMPTS_PER_STRATEGY must be between 1 and 8",
            ));
        }
        if !(1..=4).contains(&limits.max_strategies) {
            return Err(invalid("LFM3_MAX_STRATEGIES must be between 1 and 4"));
        }
        if limits.max_total_attempts < limits.max_attempts_per_strategy
            || limits.max_total_attempts > 16
        {
            return Err(invalid(
                "LFM3_MAX_TOTAL_ATTEMPTS must cover one strategy and be at most 16",
            ));
        }
        if !(10..=86_400).contains(&limits.run_timeout_seconds) {
            return Err(invalid(
                "LFM3_RUN_TIMEOUT_SECONDS must be between 10 and 86400",
            ));
        }
        if !(128..=32_768).contains(&limits.max_tokens) {
            return Err(invalid("LFM3_MAX_TOKENS must be between 128 and 32768"));
        }
        if !(0.0..=2.0).contains(&limits.temperature) || !limits.temperature.is_finite() {
            return Err(invalid(
                "LFM3_TEMPERATURE must be finite and between 0 and 2",
            ));
        }
        if !(1..=3_600).contains(&limits.shell_timeout_seconds) {
            return Err(invalid("LFM3_SHELL_TIMEOUT must be between 1 and 3600"));
        }
        if !(1_000..=1_000_000).contains(&limits.context_chars) {
            return Err(invalid(
                "LFM3_CONTEXT_CHARS must be between 1000 and 1000000",
            ));
        }
        Ok(())
    }

    /// Enforces the stricter local-only contract used by packaged applications.
    pub fn validate_local(&self, profile: Profile) -> Result<(), HarnessError> {
        self.validate()?;
        if profile != Profile::Packaged {
            return Ok(());
        }

        for (name, url) in [
            ("text", &self.text_url),
            ("vision", &self.vision_url),
            ("audio", &self.audio_url),
            ("d1", &self.d1_url),
        ] {
            if !is_loopback(url) {
                return Err(HarnessError::RemoteEndpointRejected(format!(
                    "{name} endpoint {url}"
                )));
            }
        }
        if self.allow_download {
            return Err(invalid(
                "packaged runs cannot enable model downloads; provision weights during setup",
            ));
        }
        if self.reuse_external_servers {
            return Err(invalid(
                "packaged runs must own their model servers; external reuse is development-only",
            ));
        }
        Ok(())
    }
}

/// Returns whether a URL points at an explicit loopback host.
#[must_use]
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => IpAddr::V6(address).is_loopback(),
        None => false,
    }
}

fn default_url(raw: &str) -> Url {
    Url::parse(raw).expect("default model URLs are static and valid")
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("LFM3Harness")
}

fn invalid(message: impl Into<String>) -> HarnessError {
    HarnessError::InvalidConfiguration(message.into())
}

fn parse_url(value: Option<String>, key: &str, default: Url) -> Result<Url, HarnessError> {
    let Some(raw) = value else {
        return Ok(default);
    };
    Url::parse(&raw).map_err(|error| invalid(format!("{key} is not a valid URL: {error}")))
}

fn parse_bool(value: Option<String>, key: &str, default: bool) -> Result<bool, HarnessError> {
    let Some(raw) = value else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(invalid(format!("{key} must be a boolean"))),
    }
}

fn parse_number<T>(value: Option<String>, key: &str, default: T) -> Result<T, HarnessError>
where
    T: FromStr,
    T::Err: Display,
{
    let Some(raw) = value else {
        return Ok(default);
    };
    raw.trim()
        .parse()
        .map_err(|error| invalid(format!("{key} has an invalid value: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{Config, Profile};

    #[test]
    fn environment_lookup_is_deterministic() {
        let config = Config::from_lookup(|key| match key {
            "LFM3_MAX_STEPS" => Some("4".to_owned()),
            "LFM3_PROFILE" => Some("gpu".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.limits.max_steps, 4);
        assert_eq!(config.profile, Profile::Gpu);
    }

    #[test]
    fn invalid_budget_is_rejected() {
        let mut config = Config::default();
        config.limits.max_total_attempts = 1;
        assert!(config.validate().is_err());
    }
}
