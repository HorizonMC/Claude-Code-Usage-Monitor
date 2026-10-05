#![windows_subsystem = "windows"]

mod diagnose;
mod localization;
mod models;
mod native_interop;
mod poller;
mod sysinfo;
mod theme;
mod tray_icon;
mod updater;
mod window;

/// `--profile <name>` runs a second, independent widget (own settings, mutex
/// and startup entry) — used for a system-stats-only instance on another
/// taskbar. The name is restricted to `[A-Za-z0-9_]` since it ends up in file
/// and kernel-object names.
fn profile_from_args(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--profile")
        .and_then(|i| args.get(i + 1))
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let profile = profile_from_args(&args);
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    if diagnose_enabled {
        let suffix = profile
            .as_deref()
            .map(|name| format!("-{name}"))
            .unwrap_or_default();
        match diagnose::init(&suffix) {
            Ok(path) => diagnose::log(format!("startup args={args:?} log_path={}", path.display())),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        if diagnose_enabled {
            diagnose::log(format!("cli mode exited with code {exit_code}"));
        }
        std::process::exit(exit_code);
    }

    // `--claude-config-dir <path>`: poll the Claude account logged in under that
    // config dir (e.g. %USERPROFILE%\.claude-sarocha). Exported as CLAUDE_CONFIG_DIR
    // so credential lookup, the CLI token refresh and the account label all follow it.
    if let Some(dir) = args
        .iter()
        .position(|arg| arg == "--claude-config-dir")
        .and_then(|i| args.get(i + 1))
    {
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
    }

    window::set_profile(profile);

    if diagnose_enabled {
        diagnose::log("entering window::run");
    }
    window::run();
}
