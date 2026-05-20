use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Once,
    thread,
    time::Duration,
};

const PROXY_LISTEN: &str = "127.0.0.1:1337";
static START_PROXY_SUPERVISOR: Once = Once::new();

pub fn ensure_started() -> Result<()> {
    log_missing_codex_auth();

    START_PROXY_SUPERVISOR.call_once(|| {
        thread::spawn(supervise_proxy);
    });

    Ok(())
}

fn log_missing_codex_auth() {
    let has_explicit_key = std::env::var("BYOB_OPENAI_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
        || json_file_has_key(
            &app_support_dir().unwrap_or_default().join("config.json"),
            "openai_api_key",
        );
    let has_codex_key = codex_auth_path()
        .ok()
        .is_some_and(|path| json_file_has_key(&path, "OPENAI_API_KEY"));

    if !has_explicit_key && !has_codex_key {
        log::warn!(
            "WarpOCA did not find Codex CLI auth at {}. Run `codex login` before using Agent Mode.",
            codex_auth_hint(),
        );
    }
}

fn supervise_proxy() {
    let mut child: Option<Child> = None;

    loop {
        if proxy_is_healthy() {
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        if let Some(existing) = child.as_mut() {
            match existing.try_wait() {
                Ok(None) => {
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
                Ok(Some(status)) => {
                    log::warn!("WarpOCA proxy exited with status {status}");
                    child = None;
                }
                Err(err) => {
                    log::warn!("Unable to inspect WarpOCA proxy process: {err:#}");
                    child = None;
                }
            }
        }

        match spawn_proxy() {
            Ok(spawned) => {
                log::info!("Started bundled WarpOCA proxy");
                child = Some(spawned);
            }
            Err(err) => {
                log::error!("Unable to start bundled WarpOCA proxy: {err:#}");
                thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn spawn_proxy() -> Result<Child> {
    let proxy = proxy_binary_path()?;
    let app_support = app_support_dir()?;
    let log_dir = app_support.join("logs");
    fs::create_dir_all(&log_dir)?;

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("warpoca_proxy.log"))?;
    let stdout = log_file.try_clone()?;
    let stderr = log_file;

    Command::new(proxy)
        .env("BYOB_PROXY_LISTEN", PROXY_LISTEN)
        .env("BYOB_CONFIG_PATH", app_support.join("config.json"))
        .env("PATH", helper_path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn byob_proxy")
}

fn proxy_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;

    #[cfg(target_os = "macos")]
    {
        let contents_dir = current_exe
            .parent()
            .and_then(|path| path.parent())
            .ok_or_else(|| anyhow!("failed to locate app bundle Contents directory"))?;

        let bundled = contents_dir.join("Helpers/byob_proxy");
        if bundled.exists() {
            return Ok(bundled);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let install_dir = current_exe
            .parent()
            .ok_or_else(|| anyhow!("failed to locate Windows install directory"))?;

        let bundled = install_dir.join("Helpers").join("byob_proxy.exe");
        if bundled.exists() {
            return Ok(bundled);
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let install_dir = current_exe
            .parent()
            .ok_or_else(|| anyhow!("failed to locate install directory"))?;

        let bundled = install_dir.join("byob_proxy");
        if bundled.exists() {
            return Ok(bundled);
        }
    }

    let fallback = app_support_dir()?.join(format!("byob_proxy{}", std::env::consts::EXE_SUFFIX));
    if fallback.exists() {
        return Ok(fallback);
    }

    Err(anyhow!("missing bundled proxy helper"))
}

fn proxy_is_healthy() -> bool {
    let Ok(addr) = PROXY_LISTEN.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    let mut buf = [0; 64];
    stream
        .read(&mut buf)
        .ok()
        .and_then(|read| std::str::from_utf8(&buf[..read]).ok().map(str::to_owned))
        .is_some_and(|response| response.starts_with("HTTP/1.1 200"))
}

fn helper_path() -> String {
    #[cfg(target_os = "macos")]
    {
        return "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var("PATH").unwrap_or_else(|_| {
            r"C:\Windows\System32;C:\Windows;C:\Windows\System32\WindowsPowerShell\v1.0".to_string()
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string())
    }
}

fn json_file_has_key(path: &PathBuf, key: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .and_then(|value| value.get(key).and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|value| !value.trim().is_empty())
}

fn app_support_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home).join("Library/Application Support/WarpOCA"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = nonempty_env_path("APPDATA") {
            return Ok(appdata.join("WarpOCA"));
        }
        let home = user_home_dir().context("USERPROFILE/HOME is not set")?;
        return Ok(home.join("AppData").join("Roaming").join("WarpOCA"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(data_home) = nonempty_env_path("XDG_DATA_HOME") {
            return Ok(data_home.join("WarpOCA"));
        }
        let home = user_home_dir().context("HOME is not set")?;
        Ok(home.join(".local").join("share").join("WarpOCA"))
    }
}

fn codex_auth_path() -> Result<PathBuf> {
    if let Some(codex_home) = nonempty_env_path("CODEX_HOME") {
        return Ok(codex_home.join("auth.json"));
    }

    Ok(user_home_dir()
        .context("home directory is not set")?
        .join(".codex")
        .join("auth.json"))
}

fn user_home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        nonempty_env_path("USERPROFILE").or_else(|| nonempty_env_path("HOME"))
    }

    #[cfg(not(target_os = "windows"))]
    {
        nonempty_env_path("HOME")
    }
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn codex_auth_hint() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        r"%USERPROFILE%\.codex\auth.json"
    }

    #[cfg(not(target_os = "windows"))]
    {
        "~/.codex/auth.json"
    }
}
