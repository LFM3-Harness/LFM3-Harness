//! Configuration and local-runtime diagnostics that never load weights.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    time::Duration,
};

use lfm3_harness::{CancellationContext, Config, LlamaClient};
use serde_json::{json, Map, Value};
use url::Url;

/// The llama.cpp build this harness is qualified against.
pub const PINNED_BUILD: u32 = 11_960;
pub const PINNED_RELEASE: &str = "0.8.0";

pub async fn report(probe_models: bool) -> Result<Value, String> {
    let config = Config::load().map_err(|error| error.to_string())?;
    config
        .validate_local(config.profile)
        .map_err(|error| error.to_string())?;

    let mut report = json!({
        "service": "lfm3-harness",
        "version": env!("CARGO_PKG_VERSION"),
        "host": {
            "os": env::consts::OS,
            "arch": env::consts::ARCH,
        },
        "profile": config.profile.to_string(),
        "runtime": "rust",
        "local_only": true,
        "workspace": config.workspace.as_deref().map(display_path),
        "data_dir": display_path(&config.data_dir),
        "allow_download": config.allow_download,
        "reuse_external_servers": config.reuse_external_servers,
        "endpoints": {
            "text": endpoint(&config.text_url, &config.text_model),
            "vision": endpoint(&config.vision_url, &config.vision_model),
            "audio": endpoint(&config.audio_url, &config.audio_model),
            "d1": endpoint(&config.d1_url, &config.d1_model),
        },
        "llama": {
            "server": config.llama.server.as_deref().map(display_path),
            "server_exists": config.llama.server.as_deref().is_some_and(Path::exists),
            "tts": config.llama.tts.as_deref().map(display_path),
            "tts_exists": config.llama.tts.as_deref().is_some_and(Path::exists),
            "cache": config.llama.cache.as_deref().map(display_path),
        },
        "limits": {
            "max_steps": config.limits.max_steps,
            "max_attempts_per_strategy": config.limits.max_attempts_per_strategy,
            "max_strategies": config.limits.max_strategies,
            "max_total_attempts": config.limits.max_total_attempts,
            "run_timeout_seconds": config.limits.run_timeout_seconds,
        }
    });

    if probe_models {
        report["probe"] = probe(&config).await;
    }
    Ok(report)
}

/// Identity and address of one model role.
struct Role<'a> {
    name: &'static str,
    weight_token: &'static str,
    env_key: &'static str,
    url: &'a Url,
    model: &'a str,
}

fn roles(config: &Config) -> [Role<'_>; 4] {
    [
        Role {
            name: "text",
            weight_token: "LFM3-1.7B",
            env_key: "LFM3_TEXT_URL",
            url: &config.text_url,
            model: &config.text_model,
        },
        Role {
            name: "vision",
            weight_token: "LFM3-VL-2B",
            env_key: "LFM3_VISION_URL",
            url: &config.vision_url,
            model: &config.vision_model,
        },
        Role {
            name: "audio",
            weight_token: "LFM3-Audio-2B",
            env_key: "LFM3_AUDIO_URL",
            url: &config.audio_url,
            model: &config.audio_model,
        },
        Role {
            name: "d1",
            weight_token: "d1-2B",
            env_key: "LFM3_D1_URL",
            url: &config.d1_url,
            model: &config.d1_model,
        },
    ]
}

pub async fn probe(config: &Config) -> Value {
    let runtime = probe_runtime(config);
    let (weights_root, weights) = scan_weights();
    let mut endpoints = Map::new();
    let mut setup = Vec::new();
    let mut ready = true;

    for role in roles(config) {
        let cancel = CancellationContext::new(Duration::from_secs(2));
        let client = LlamaClient::new(role.url.clone(), role.model);
        let (entry, available) = match client {
            Ok(client) => match client.list_models(&cancel.child()).await {
                Ok(identities) => {
                    let available = identities.iter().any(|id| id == role.model);
                    (
                        json!({
                            "url": role.url.as_str(),
                            "expected": role.model,
                            "reachable": true,
                            "available": available,
                            "identities": identities,
                        }),
                        available,
                    )
                }
                Err(error) => (
                    json!({
                        "url": role.url.as_str(),
                        "expected": role.model,
                        "reachable": false,
                        "available": false,
                        "error": error.to_string(),
                    }),
                    false,
                ),
            },
            Err(error) => (
                json!({
                    "url": role.url.as_str(),
                    "expected": role.model,
                    "reachable": false,
                    "available": false,
                    "error": error.to_string(),
                }),
                false,
            ),
        };
        ready &= available;
        if !available {
            setup.push(setup_instruction(&role, &weights_root, &weights, &runtime));
        }
        endpoints.insert(role.name.to_owned(), entry);
    }

    json!({
        "runtime": runtime,
        "weights_root": display_path(&weights_root),
        "weights": weights,
        "endpoints": endpoints,
        "ready": ready,
        "setup": setup,
    })
}

pub fn probe_runtime(config: &Config) -> Value {
    let server = config.llama.server.clone().or_else(default_server_path);
    let Some(server) = server else {
        return json!({
            "path": Value::Null,
            "expected": format!("b{PINNED_BUILD} / v{PINNED_RELEASE}"),
            "pinned": false,
            "error": "no llama-server binary found; set LFM3_LLAMA_SERVER to the pinned b11960 build",
        });
    };
    let output = ProcessCommand::new(&server).arg("--version").output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            // llama-server writes its version banner to stderr, other builds
            // use stdout; accept either.
            let lines = stdout.lines().chain(stderr.lines()).map(str::trim);
            let line = lines
                .clone()
                .find(|line| line.starts_with("version:"))
                .or_else(|| lines.clone().find(|line| !line.is_empty()))
                .unwrap_or("")
                .to_owned();
            let build = parse_build(&line);
            let release = parse_release(&line);
            let pinned = build == Some(PINNED_BUILD)
                && release
                    .as_deref()
                    .is_some_and(|value| value.starts_with(PINNED_RELEASE));
            json!({
                "path": display_path(&server),
                "version": line,
                "build": build,
                "release": release,
                "expected": format!("b{PINNED_BUILD} / v{PINNED_RELEASE}"),
                "pinned": pinned,
            })
        }
        Ok(output) => json!({
            "path": display_path(&server),
            "expected": format!("b{PINNED_BUILD} / v{PINNED_RELEASE}"),
            "pinned": false,
            "error": format!("llama-server --version exited with {}", output.status),
            "stderr": String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
        Err(error) => json!({
            "path": display_path(&server),
            "expected": format!("b{PINNED_BUILD} / v{PINNED_RELEASE}"),
            "pinned": false,
            "error": format!("could not run llama-server: {error}"),
        }),
    }
}

pub fn default_server_path() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let candidate = Path::new("tools").join("llama").join(executable);
    candidate.exists().then_some(candidate)
}

fn parse_release(line: &str) -> Option<String> {
    let rest = line.strip_prefix("version:").unwrap_or(line).trim();
    let release = rest.split(' ').next()?.trim();
    (!release.is_empty()).then(|| release.to_owned())
}

fn parse_build(line: &str) -> Option<u32> {
    let (_, after) = line.split_once("(build ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Collects available GGUF weights without ever downloading anything.
pub fn scan_weights() -> (PathBuf, Vec<Value>) {
    let root = env::var_os("LFM3_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models"));
    let mut files = Vec::new();
    collect_gguf(&root, &root, 0, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let files = files
        .into_iter()
        .map(|(path, size)| json!({"path": path, "size_bytes": size}))
        .collect();
    (root, files)
}

/// Finds one GGUF matching a weight token, preferring the non-projector file.
pub fn find_weight(root: &Path, weights: &[Value], token: &str) -> Option<PathBuf> {
    let mut main = None;
    let mut mmproj = None;
    for weight in weights {
        let Some(path) = weight.get("path").and_then(Value::as_str) else {
            continue;
        };
        if !path
            .to_ascii_lowercase()
            .contains(&token.to_ascii_lowercase())
        {
            continue;
        }
        if path.to_ascii_lowercase().contains("mmproj") {
            mmproj.get_or_insert(path);
        } else {
            main.get_or_insert(path);
        }
    }
    main.or(mmproj).map(|relative| root.join(relative))
}

fn collect_gguf(root: &Path, directory: &Path, depth: usize, files: &mut Vec<(String, u64)>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_gguf(root, &path, depth + 1, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
            && path.is_file()
        {
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            files.push((relative, size));
        }
    }
}

fn setup_instruction(role: &Role<'_>, root: &Path, weights: &[Value], runtime: &Value) -> String {
    let server = runtime.get("path").and_then(Value::as_str);
    let server_missing = server.is_none();

    let mut main = None;
    let mut mmproj = None;
    for weight in weights {
        let Some(path) = weight.get("path").and_then(Value::as_str) else {
            continue;
        };
        if !path
            .to_ascii_lowercase()
            .contains(&role.weight_token.to_ascii_lowercase())
        {
            continue;
        }
        if path.to_ascii_lowercase().contains("mmproj") {
            mmproj.get_or_insert(path);
        } else {
            main.get_or_insert(path);
        }
    }

    let host = role.url.host_str().unwrap_or("127.0.0.1");
    let port = role.url.port_or_known_default().unwrap_or(8080);
    let launch = match (server, main) {
        (Some(server), Some(main)) => {
            let mut command = format!(
                "\"{server}\" -m \"{}\" --host {host} --port {port} --alias {} --jinja",
                root.join(main).display(),
                role.model
            );
            if let Some(mmproj) = mmproj {
                command.push_str(&format!(" --mmproj \"{}\"", root.join(mmproj).display()));
            }
            Some(command)
        }
        _ => None,
    };

    let mut instruction = match (launch, server_missing) {
        (Some(command), _) => format!(
            "{} ({}): start the pinned server with `{command}`, or point {} at an already-running local server.",
            role.name, role.model, role.env_key
        ),
        (None, true) => format!(
            "{} ({}): install the pinned llama.cpp b{} / v{} build (set LFM3_LLAMA_SERVER) and provision the {} GGUF under {}; this harness never downloads weights.",
            role.name,
            role.model,
            PINNED_BUILD,
            PINNED_RELEASE,
            role.weight_token,
            display_path(root)
        ),
        (None, false) => format!(
            "{} ({}): provision the {} GGUF under {} and start llama-server, or point {} at an already-running local server; this harness never downloads weights.",
            role.name,
            role.model,
            role.weight_token,
            display_path(root),
            role.env_key
        ),
    };
    if runtime
        .get("pinned")
        .and_then(Value::as_bool)
        .is_some_and(|pinned| !pinned)
        && !server_missing
    {
        instruction.push_str(&format!(
            " The detected llama-server is not b{PINNED_BUILD} / v{PINNED_RELEASE}; qualify or replace it before running."
        ));
    }
    instruction
}

fn endpoint(url: &Url, model: &str) -> Value {
    json!({
        "url": url.as_str(),
        "model": model,
    })
}

pub fn display_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_build, parse_release};

    #[test]
    fn version_lines_are_parsed() {
        let line = "version: 0.8.0-dev (build 11960, commit 8642541b2)";
        assert_eq!(parse_release(line).as_deref(), Some("0.8.0-dev"));
        assert_eq!(parse_build(line), Some(11_960));
    }

    #[test]
    fn unexpected_version_lines_do_not_panic() {
        assert_eq!(parse_release("unknown"), Some("unknown".to_owned()));
        assert_eq!(parse_build("version: 1.0.0"), None);
    }
}
