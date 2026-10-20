//! PulseAudio/PipeWire control helpers for the virtual microphone.
//!
//! `vcam` does not link against libpulse on purpose: the daemon only needs to
//! create and remove two session modules, which is exactly what `pactl` does.
//! Shelling out keeps the build free of an extra C dependency and works the
//! same on PulseAudio and on PipeWire's PulseAudio compatibility server.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Remedy printed when the PulseAudio/PipeWire control client is missing.
pub const PACTL_REMEDY: &str =
    "pactl was not found; install pulseaudio-utils (PulseAudio) or pipewire-pulse (PipeWire)";

const PACTL: &str = "pactl";
/// A broken sound server must not stall daemon startup or a status query.
const PACTL_TIMEOUT: Duration = Duration::from_secs(5);

/// One entry of `pactl list short modules`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    pub id: u32,
    pub name: String,
    pub args: String,
}

/// Parse `pactl list short modules` output.
///
/// Each line is `id<TAB>name<TAB>args`; the argument column is optional and may
/// itself contain spaces.
pub fn parse_modules(text: &str) -> Vec<ModuleEntry> {
    let mut modules = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let Some(id) = fields.next().and_then(|id| id.trim().parse::<u32>().ok()) else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        modules.push(ModuleEntry {
            id,
            name: name.trim().to_string(),
            args: fields.next().unwrap_or("").trim().to_string(),
        });
    }
    modules
}

/// Which sound server answered `pactl info`: `pipewire`, `pulseaudio`, or
/// `unknown` when the output does not name one.
pub fn parse_backend(info: &str) -> String {
    for line in info.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("Server Name") {
            continue;
        }
        let value = value.trim().to_ascii_lowercase();
        if value.contains("pipewire") {
            return "pipewire".to_string();
        }
        if value.contains("pulseaudio") {
            return "pulseaudio".to_string();
        }
    }
    "unknown".to_string()
}

/// Escape a value for a `*_properties=` module argument.
///
/// PipeWire's PulseAudio server splits module arguments on spaces and does not
/// honour plain quotes around a value, so spaces are escaped explicitly and
/// the value is wrapped in double quotes. Without this, `Virtual Microphone`
/// would be stored as `Virtual`.
pub fn escape_property_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            ' ' => escaped.push_str("\\ "),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

/// Return the value of a `key=value` token inside a module's argument string.
///
/// Values used for identification (`sink_name`, `source_name`) never contain
/// spaces, so a simple token scan is enough and avoids interpreting the
/// quoting used by `sink_properties`.
pub fn module_arg<'a>(args: &'a str, key: &str) -> Option<&'a str> {
    args.split_whitespace().find_map(|token| {
        let (token_key, value) = token.split_once('=')?;
        (token_key == key).then_some(value)
    })
}

/// Find a loaded module by name and one identifying `key=value` argument.
pub fn find_module<'a>(
    modules: &'a [ModuleEntry],
    name: &str,
    key: &str,
    value: &str,
) -> Option<&'a ModuleEntry> {
    modules
        .iter()
        .find(|module| module.name == name && module_arg(&module.args, key) == Some(value))
}

fn run_pactl(args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(PACTL)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PACTL_REMEDY.to_string()
            } else {
                format!("failed to run pactl: {error}")
            }
        })?;

    let deadline = Instant::now() + PACTL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("failed to read pactl output: {error}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    return Err(if stderr.is_empty() {
                        format!("pactl {} failed", args.join(" "))
                    } else {
                        stderr
                    });
                }
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "pactl {} timed out after {}s",
                    args.join(" "),
                    PACTL_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("failed to wait for pactl: {error}")),
        }
    }
}

/// Read `pactl info`; the error explains why the sound server is unreachable.
pub fn server_info() -> Result<String, String> {
    run_pactl(&["info"])
}

/// List the modules currently loaded into the sound server.
pub fn modules() -> Result<Vec<ModuleEntry>, String> {
    run_pactl(&["list", "short", "modules"]).map(|text| parse_modules(&text))
}

/// Whether a capture source with this name is currently exposed.
pub fn source_exists(name: &str) -> Result<bool, String> {
    let text = run_pactl(&["list", "short", "sources"])?;
    Ok(text.lines().any(|line| {
        line.split('\t')
            .nth(1)
            .map(|candidate| candidate.trim() == name)
            .unwrap_or(false)
    }))
}

/// Load a module and return its module index.
pub fn load_module(module: &str, args: &[String]) -> Result<u32, String> {
    let mut command = vec!["load-module", module];
    command.extend(args.iter().map(String::as_str));
    let output = run_pactl(&command)?;
    output
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("unexpected pactl load-module output: {}", output.trim()))
}

/// Unload a module by index. Unloading an already removed module is an error
/// in pactl, so callers treat failures as non-fatal.
pub fn unload_module(id: u32) -> Result<(), String> {
    run_pactl(&["unload-module", &id.to_string()]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULES: &str = "0\tmodule-always-sink\t\n\
1\tmodule-null-sink\tsink_name=vcam_sink sink_properties=device.description=\"Virtual Microphone\"\n\
2\tmodule-remap-source\tmaster=vcam_sink.monitor source_name=vcam_mic source_properties=device.description=\"Virtual Microphone\"\n";

    #[test]
    fn parses_module_list() {
        let modules = parse_modules(MODULES);
        assert_eq!(modules.len(), 3);
        assert_eq!(modules[0].name, "module-always-sink");
        assert_eq!(modules[1].id, 1);
        assert_eq!(module_arg(&modules[1].args, "sink_name"), Some("vcam_sink"));
        assert_eq!(
            find_module(&modules, "module-remap-source", "source_name", "vcam_mic")
                .map(|module| module.id),
            Some(2)
        );
        assert!(find_module(&modules, "module-null-sink", "sink_name", "other").is_none());
    }

    #[test]
    fn recognises_the_sound_server() {
        assert_eq!(
            parse_backend("Server Name: PulseAudio (on PipeWire 1.0.5)\nServer Version: 1.0.5"),
            "pipewire"
        );
        assert_eq!(
            parse_backend("Server Name: pulseaudio\nServer Version: 16.1"),
            "pulseaudio"
        );
        assert_eq!(parse_backend("Server Name: something else"), "unknown");
    }

    #[test]
    fn ignores_malformed_module_lines() {
        assert!(parse_modules("not a module\n\n").is_empty());
    }

    #[test]
    fn escapes_property_values() {
        assert_eq!(
            escape_property_value("Virtual Microphone"),
            "\"Virtual\\ Microphone\""
        );
        assert_eq!(escape_property_value("say \"hi\""), "\"say\\ \\\"hi\\\"\"");
    }
}
