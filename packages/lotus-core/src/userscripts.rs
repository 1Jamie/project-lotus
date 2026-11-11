//! Track-1 renderer userscripts (compile-time embedded).
//!
//! Scripts are `include_str!`-embedded so packaged SEA / arbitrary-cwd launches
//! never fail path resolution. Injection is gated by [`PaneScriptRole`]:
//! - remote/sandbox panes get nothing (no host token);
//! - host recovery chrome (crash / select) get IPC only;
//! - trusted app panes get the full Track-1 pipeline.

use std::rc::Rc;

use servo::{UserContentManager, UserScript};

use crate::window::ScrollbarConfig;

pub const IPC_BOOTSTRAP: &str = include_str!("userscripts/ipc_bootstrap.js");
pub const DRAG_REGION: &str = include_str!("userscripts/drag_region.js");
pub const SCROLLBAR: &str = include_str!("userscripts/scrollbar.js");

/// Which kind of pane is receiving userscripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneScriptRole {
    /// Trusted in-process app pane — full Track-1 pipeline.
    Track1AppPane,
    /// `__lotus_select*` overlay — host chrome; IPC only (no drag/scrollbar).
    SelectOverlay,
    /// `__lotus_crash*` / other `__lotus_*` host recovery chrome — IPC only.
    CrashChrome,
    /// Out-of-process / `sandbox: true` remote pane (SHM path).
    RemoteSandbox,
}

/// Classify a pane id (+ remote flag) into a script role.
pub fn classify_pane_role(pane_id: &str, is_remote_sandbox: bool) -> PaneScriptRole {
    if is_remote_sandbox {
        return PaneScriptRole::RemoteSandbox;
    }
    if pane_id.starts_with("__lotus_select") {
        return PaneScriptRole::SelectOverlay;
    }
    if pane_id.starts_with("__lotus_") {
        return PaneScriptRole::CrashChrome;
    }
    PaneScriptRole::Track1AppPane
}

/// Environment for Track-1 UserContentManager population.
pub struct PaneScriptEnv<'a> {
    pub role: PaneScriptRole,
    pub msgpackr_source: &'a str,
    pub origins_json: &'a str,
    pub port: u16,
    pub token: &'a str,
    pub window_id: &'a str,
    pub pane_id: &'a str,
    pub theme: Option<&'a str>,
    pub scrollbar: Option<&'a ScrollbarConfig>,
}

fn inject_host_ipc(ucm: &UserContentManager, env: &PaneScriptEnv<'_>) -> usize {
    let mut added = 0usize;

    ucm.add_script(Rc::new(UserScript::from(env.msgpackr_source)));
    added += 1;
    ucm.add_script(Rc::new(UserScript::from(IPC_BOOTSTRAP)));
    added += 1;

    // Host chrome pages are always lotus-resource://; still gate on trusted origins
    // so a mis-routed document cannot inherit the session token.
    let port_script = format!(
        r#"(function() {{
            const trusted = {};
            const loc = window.location;
            const effectiveOrigin = (loc.origin && loc.origin !== 'null') ? loc.origin : (loc.protocol + '//' + (loc.host || 'localhost'));
            if (trusted.indexOf(effectiveOrigin) !== -1 || (loc.protocol === 'lotus-resource:' && trusted.indexOf('lotus-resource://localhost') !== -1)) {{
                window.lotus = window.lotus || {{}};
                window.lotus.port = {};
                window.lotus.token = '{}';
                window.lotus.id = '{}';
                window.lotus.paneId = '{}';
                // Open IPC immediately so host→renderer pushes (stats, mode) are received
                // without waiting for the page to call lotus.send() first.
                try {{
                    if (typeof window.lotus._connectWs === 'function') {{
                        window.lotus._connectWs();
                    }}
                }} catch (e) {{}}
            }}
        }})();"#,
        env.origins_json, env.port, env.token, env.window_id, env.pane_id
    );
    ucm.add_script(Rc::new(UserScript::from(port_script.as_str())));
    added += 1;

    added
}

/// Populate a pane's `UserContentManager` with Track-1 / host-chrome scripts.
///
/// Remote sandbox panes receive nothing. Host recovery chrome (crash + select)
/// receive IPC bootstrap only — required for `crash-info` / Respawn and select
/// populate handshakes. Trusted app panes get the full pipeline.
pub fn populate_track1_ucm(ucm: &UserContentManager, env: &PaneScriptEnv<'_>) -> usize {
    match env.role {
        PaneScriptRole::RemoteSandbox => 0,
        PaneScriptRole::SelectOverlay | PaneScriptRole::CrashChrome => inject_host_ipc(ucm, env),
        PaneScriptRole::Track1AppPane => {
            let mut added = inject_host_ipc(ucm, env);

            if let Some(theme) = env.theme {
                let theme_script = format!(
                    r#"
            window.lotus.theme = '{}';
            try {{ document.documentElement.dataset.theme = window.lotus.theme; }} catch(e) {{}}
        "#,
                    theme
                );
                ucm.add_script(Rc::new(UserScript::from(theme_script.as_str())));
                added += 1;
            }

            ucm.add_script(Rc::new(UserScript::from(DRAG_REGION)));
            added += 1;

            if let Some(cfg) = env.scrollbar {
                if cfg.enabled {
                    let cfg_script = format!(
                        "window.__LOTUS_SCROLLBAR_CONFIG__ = {{ theme: '{}', autoHide: {}, width: {} }};",
                        cfg.theme, cfg.auto_hide, cfg.width
                    );
                    ucm.add_script(Rc::new(UserScript::from(cfg_script.as_str())));
                    ucm.add_script(Rc::new(UserScript::from(SCROLLBAR)));
                    added += 2;
                }
            }

            added
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_roles() {
        assert_eq!(classify_pane_role("main", false), PaneScriptRole::Track1AppPane);
        assert_eq!(
            classify_pane_role("__lotus_select_overlay", false),
            PaneScriptRole::SelectOverlay
        );
        assert_eq!(
            classify_pane_role("__lotus_crash_cover", false),
            PaneScriptRole::CrashChrome
        );
        assert_eq!(
            classify_pane_role("main", true),
            PaneScriptRole::RemoteSandbox
        );
    }
}
