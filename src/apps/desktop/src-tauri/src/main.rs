#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let options = agent_hud_host::HostOptions {
        canonical_executable: hook_owner(),
    };
    agent_hud_host::run_with_options(tauri::generate_context!(), None, options);
}

/// The installed copy owns the client hooks: they run this executable, so a
/// release build names itself as their fixed owner. A debug build from the
/// source tree leaves the hooks to the installed app.
fn hook_owner() -> Option<std::path::PathBuf> {
    if cfg!(debug_assertions) {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let executable = executable.canonicalize().unwrap_or(executable);
    Some(plain_path(executable))
}

/// `canonicalize` gives Windows paths the `\\?\` prefix, which the commands
/// written into client hook settings must not carry.
fn plain_path(path: std::path::PathBuf) -> std::path::PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => std::path::PathBuf::from(rest),
        _ => path,
    }
}
