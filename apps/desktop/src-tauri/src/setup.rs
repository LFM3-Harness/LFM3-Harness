//! Read-only first-run discovery for local llama.cpp assets.
//!
//! Setup never downloads anything. It locates an already installed pinned
//! runtime, GGUF weight directory, TTS binary, and projector, records the
//! user's absolute choices in `setup.json` under trusted application data, and
//! reports every missing or invalid item so the UI can ask for it explicitly.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use lfm3_harness::HarnessError;
use serde::{Deserialize, Serialize};

pub const PINNED_BUILD: u32 = 11_960;
pub const PINNED_RELEASE: &str = "0.8.0";
const SETTINGS_FILE: &str = "setup.json";

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SetupSettings {
    pub llama_server: Option<PathBuf>,
    pub llama_tts: Option<PathBuf>,
    pub models_dir: Option<PathBuf>,
    pub projector: Option<PathBuf>,
}

impl SetupSettings {
    /// Loads the user's choices. A malformed file fails closed; it is never
    /// silently replaced.
    pub fn load(data_dir: &Path) -> Result<Self, HarnessError> {
        let path = data_dir.join(SETTINGS_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => {
                return Err(HarnessError::Storage(format!(
                    "read setup {}: {error}",
                    path.display()
                )))
            }
        };
        let settings: Self = serde_json::from_str(&raw)
            .map_err(|error| HarnessError::Storage(format!("parse {}: {error}", path.display())))?;
        settings.validate()?;
        Ok(settings)
    }

    /// Persists the choices after validating every supplied path. Only an
    /// explicit user action reaches this function.
    pub fn save(&self, data_dir: &Path) -> Result<PathBuf, HarnessError> {
        self.validate()?;
        fs::create_dir_all(data_dir).map_err(|error| {
            HarnessError::Storage(format!("create {}: {error}", data_dir.display()))
        })?;
        let path = data_dir.join(SETTINGS_FILE);
        let temporary = data_dir.join(format!("{SETTINGS_FILE}.tmp"));
        let raw = serde_json::to_string_pretty(self)
            .map_err(|error| HarnessError::Storage(format!("serialize setup: {error}")))?;
        fs::write(&temporary, raw).map_err(|error| {
            HarnessError::Storage(format!("write {}: {error}", temporary.display()))
        })?;
        fs::rename(&temporary, &path).map_err(|error| {
            HarnessError::Storage(format!("replace {}: {error}", path.display()))
        })?;
        Ok(path)
    }

    /// Rejects relative, missing, or wrongly typed paths without touching
    /// anything outside the named file.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if let Some(path) = &self.llama_server {
            validate_file(path, "llama_server")?;
        }
        if let Some(path) = &self.llama_tts {
            validate_file(path, "llama_tts")?;
        }
        if let Some(path) = &self.projector {
            validate_file(path, "projector")?;
        }
        if let Some(path) = &self.models_dir {
            if !path.is_absolute() {
                return Err(HarnessError::InvalidConfiguration(format!(
                    "models_dir must be absolute: {}",
                    path.display()
                )));
            }
            if !path.is_dir() {
                return Err(HarnessError::InvalidConfiguration(format!(
                    "models_dir is not a directory: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

fn validate_file(path: &Path, label: &str) -> Result<(), HarnessError> {
    if !path.is_absolute() {
        return Err(HarnessError::InvalidConfiguration(format!(
            "{label} must be an absolute path: {}",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(HarnessError::InvalidConfiguration(format!(
            "{label} is not a file: {}",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerProbe {
    pub build: u32,
    pub release: String,
    pub pinned: bool,
}

/// A read-only inventory of what is installed. `issues` is user-facing and
/// never triggers a download.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SetupReport {
    pub llama_server: Option<PathBuf>,
    pub server: Option<ServerProbe>,
    pub llama_tts: Option<PathBuf>,
    pub models_dir: Option<PathBuf>,
    pub text_weight: Option<PathBuf>,
    pub projector: Option<PathBuf>,
    pub issues: Vec<String>,
}

/// Resolves the effective settings without writing or downloading anything.
#[must_use]
pub fn inspect(settings: &SetupSettings) -> SetupReport {
    let mut report = SetupReport::default();

    let server = settings
        .llama_server
        .clone()
        .or_else(find_server_executable);
    if let Some(path) = server {
        match probe_server(&path) {
            Ok(probe) => {
                if !probe.pinned {
                    report.issues.push(format!(
                        "llama-server is not the pinned b{PINNED_BUILD} / v{PINNED_RELEASE} build"
                    ));
                }
                report.server = Some(probe);
            }
            Err(error) => report.issues.push(error),
        }
        report.llama_server = Some(path);
    } else {
        report
            .issues
            .push("no llama-server executable was found".to_owned());
    }

    report.llama_tts = settings.llama_tts.clone().or_else(find_tts_executable);
    if report.llama_tts.is_none() {
        report
            .issues
            .push("no llama-tts executable was found".to_owned());
    }

    report.models_dir = settings.models_dir.clone().or_else(find_models_dir);
    match &report.models_dir {
        Some(root) => {
            report.text_weight = find_weight(root, "LFM3-1.7B");
            if report.text_weight.is_none() {
                report
                    .issues
                    .push(format!("no LFM3-1.7B GGUF weight under {}", root.display()));
            }
            report.projector = settings.projector.clone().or_else(|| find_projector(root));
            if report.projector.is_none() {
                report
                    .issues
                    .push("no vision projector was found".to_owned());
            }
        }
        None => report
            .issues
            .push("no models directory was found".to_owned()),
    }
    report
}

/// Runs the pinned binary's read-only `--version` probe.
pub fn probe_server(executable: &Path) -> Result<ServerProbe, String> {
    let mut command = Command::new(executable);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .map_err(|error| format!("could not run llama-server: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines = stdout.lines().chain(stderr.lines()).map(str::trim);
    let banner = lines
        .clone()
        .find(|line| line.starts_with("version:"))
        .or_else(|| lines.clone().find(|line| !line.is_empty()))
        .unwrap_or("")
        .to_owned();
    let build = parse_build(&banner).unwrap_or(0);
    let release = parse_release(&banner).unwrap_or_default();
    let pinned = build == PINNED_BUILD && release.starts_with(PINNED_RELEASE);
    Ok(ServerProbe {
        build,
        release,
        pinned,
    })
}

fn parse_release(banner: &str) -> Option<String> {
    let rest = banner.strip_prefix("version:").unwrap_or(banner).trim();
    rest.split(' ').next().map(str::to_owned)
}

fn parse_build(banner: &str) -> Option<u32> {
    let (_, after) = banner.split_once("(build ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[must_use]
pub fn find_server_executable() -> Option<PathBuf> {
    find_binary("llama-server")
}

#[must_use]
pub fn find_tts_executable() -> Option<PathBuf> {
    find_binary("llama-tts")
}

fn find_binary(stem: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    };
    let env_key = if stem == "llama-tts" {
        "LFM3_LLAMA_TTS"
    } else {
        "LFM3_LLAMA_SERVER"
    };
    if let Some(raw) = std::env::var_os(env_key) {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut roots = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            roots.push(directory.to_path_buf());
        }
    }
    if let Ok(directory) = std::env::current_dir() {
        roots.push(directory);
    }
    for mut directory in roots {
        for _ in 0..8 {
            for relative in [
                PathBuf::from("tools").join("llama").join(&name),
                PathBuf::from(&name),
            ] {
                let candidate = directory.join(relative);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            directory = parent.to_path_buf();
        }
    }
    None
}

#[must_use]
pub fn find_models_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("LFM3_MODELS_DIR") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return Some(path);
        }
    }
    let mut directory = std::env::current_dir().ok()?;
    for _ in 0..8 {
        let candidate = directory.join("models");
        if candidate.is_dir() {
            return Some(candidate);
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent.to_path_buf();
    }
    None
}

/// Largest GGUF whose file name contains `token`, searched without following
/// reparse points.
#[must_use]
pub fn find_weight(root: &Path, token: &str) -> Option<PathBuf> {
    let mut pending = vec![(root.to_path_buf(), 0_u32)];
    let mut best: Option<(u64, PathBuf)> = None;
    while let Some((directory, depth)) = pending.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push((path, depth + 1));
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if !name.to_ascii_lowercase().ends_with(".gguf") || !name.contains(token) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let size = metadata.len();
            if best.as_ref().is_none_or(|(current, _)| size > *current) {
                best = Some((size, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

#[must_use]
pub fn find_projector(root: &Path) -> Option<PathBuf> {
    find_projector_for(root, "mmproj")
}

/// Finds the multimodal projector whose file name also contains `token`, so
/// the vision and audio projectors cannot be confused for one another.
#[must_use]
pub fn find_projector_for(root: &Path, token: &str) -> Option<PathBuf> {
    let token = token.to_ascii_lowercase();
    let mut pending = vec![(root.to_path_buf(), 0_u32)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push((path, depth + 1));
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if name.ends_with(".gguf") && name.contains("mmproj") && name.contains(&token) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_rejected_before_use() {
        let settings = SetupSettings {
            llama_server: Some(PathBuf::from("llama-server.exe")),
            ..SetupSettings::default()
        };
        let error = settings.validate().expect_err("relative binary rejected");
        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn paths_containing_spaces_are_accepted() {
        let directory = tempfile::tempdir().expect("temp dir");
        let spaced = directory.path().join("Program Files With Spaces");
        fs::create_dir_all(&spaced).expect("spaced dir");
        let server = spaced.join("llama-server.exe");
        fs::write(&server, b"stub").expect("stub binary");
        let settings = SetupSettings {
            llama_server: Some(server.clone()),
            models_dir: Some(spaced.clone()),
            ..SetupSettings::default()
        };
        settings
            .validate()
            .expect("absolute paths with spaces are valid");
    }

    #[test]
    fn missing_weights_are_reported_and_never_created() {
        let directory = tempfile::tempdir().expect("temp dir");
        let models = directory.path().join("models");
        fs::create_dir_all(&models).expect("models dir");
        let settings = SetupSettings {
            models_dir: Some(models.clone()),
            llama_server: None,
            ..SetupSettings::default()
        };
        let report = inspect(&settings);
        assert!(report.text_weight.is_none());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.contains("LFM3-1.7B")));
        assert_eq!(
            fs::read_dir(&models).expect("read models").count(),
            0,
            "setup must never download or create weights"
        );
    }

    #[test]
    fn settings_round_trip_only_absolute_existing_paths() {
        let directory = tempfile::tempdir().expect("temp dir");
        let models = directory.path().join("weights folder");
        fs::create_dir_all(&models).expect("models");
        let settings = SetupSettings {
            models_dir: Some(models.clone()),
            ..SetupSettings::default()
        };
        let path = settings.save(directory.path()).expect("save setup");
        assert!(path.ends_with(SETTINGS_FILE));
        let loaded = SetupSettings::load(directory.path()).expect("load setup");
        assert_eq!(loaded.models_dir, Some(models));
    }
}
