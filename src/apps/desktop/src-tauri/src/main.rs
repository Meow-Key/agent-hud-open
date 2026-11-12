#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The installed copy owns client hooks and HUD tools: on Windows, the one
/// the installer puts in %LOCALAPPDATA%\Agent HUD.
fn canonical_executable() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let path = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("Agent HUD")
            .join("Agent HUD.exe");
        return path.is_file().then_some(path);
    }
    #[allow(unreachable_code)]
    None
}

fn main() {
    agent_hud_host::run_with_options(
        tauri::generate_context!(),
        None,
        agent_hud_host::HostOptions {
            canonical_executable: canonical_executable(),
        },
    );
}