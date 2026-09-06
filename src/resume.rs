//! Session identity reported by the CLI occupying a pane, including /resume.
use crate::{state, tmux};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub provider: String,
    pub id: String,
}

impl Session {
    pub fn parse(raw: &str) -> Option<Self> {
        let session: Self = serde_json::from_str(raw).ok()?;
        let valid = match session.provider.as_str() {
            "claude" => {
                !session.id.is_empty()
                    && session.id.len() <= 64
                    && session
                        .id
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() || c == '-')
            }
            "opencode" => {
                session.id.starts_with("ses_")
                    && session.id.len() <= 128
                    && session
                        .id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            _ => false,
        };
        valid.then_some(session)
    }
}

pub fn report_claude(id: &str) -> Result<()> {
    if std::env::var_os("CORC_MANAGED").is_none() {
        return Ok(());
    }
    let Ok(pane) = std::env::var("TMUX_PANE") else {
        return Ok(());
    };
    let session = Session {
        provider: "claude".into(),
        id: id.into(),
    };
    tmux::report_session(&pane, &session)
}

/// Install only in corc's state directory and merge through the child CLI's
/// environment. Array overrides must preserve the user's plugin directives.
pub fn opencode_env() -> Result<String> {
    let root = state::state_dir()?.join("opencode-resume");
    state::write_if_changed(
        &root.join("package.json"),
        r#"{"name":"corc-resume","type":"module","exports":{"./tui":"./tui.ts"}}"#,
    )?;
    state::write_if_changed(&root.join("tui.ts"), include_str!("resume-opencode.ts"))?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    let global = match std::fs::read_to_string(config_home.join("opencode/cli.json")) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };
    let mut config: serde_json::Value = match std::env::var("OPENCODE_CLI_CONFIG_CONTENT") {
        Ok(raw) => serde_json::from_str(&raw)?,
        Err(_) => serde_json::json!({}),
    };
    anyhow::ensure!(
        config.is_object(),
        "OpenCode inline CLI config must be an object"
    );
    let mut plugins = config
        .get("plugins")
        .or_else(|| global.get("plugins"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let plugin = serde_json::json!(root);
    if !plugins.contains(&plugin) {
        plugins.push(plugin);
    }
    config["plugins"] = plugins.into();
    Ok(format!("OPENCODE_CLI_CONFIG_CONTENT={config}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use std::path::Path;

    #[test]
    fn resuming_imports_history_and_transfers_existing_sessions_without_duplicates() {
        for (provider, pending, first, second) in [
            (
                "opencode",
                "pending-opencode-123",
                "ses_first",
                "ses_second",
            ),
            ("claude", "1111-1111", "2222-2222", "3333-3333"),
        ] {
            let mut state = State::default();
            state.add_conversation(
                pending.into(),
                "/work/a".into(),
                "%1".into(),
                provider.into(),
            );
            let report = |id: &str| {
                Session::parse(&serde_json::json!({"provider":provider,"id":id}).to_string())
                    .unwrap()
            };
            assert_eq!(
                state.resume_in_pane("%1", &report(first)),
                Some(pending.into())
            );
            assert_eq!(
                state.conversation(first).unwrap().pane_id.as_deref(),
                Some("%1")
            );
            state.conversation_mut(first).unwrap().content_seen = true;
            state.relocate(first, Path::new("/work/b"));

            state.add_conversation(
                second.into(),
                "/work/c".into(),
                "%2".into(),
                provider.into(),
            );
            state.conversation_mut(second).unwrap().pinned = true;
            state.resume_in_pane("%1", &report(second)).unwrap();
            assert!(state.conversation(first).unwrap().pane_id.is_none());
            assert!(state.conversation(first).unwrap().content_seen);
            let target = state.conversation(second).unwrap();
            assert_eq!(target.cwd, Path::new("/work/c"));
            assert!(target.pinned);
            assert_eq!(target.pane_id.as_deref(), Some("%1"));
            assert_eq!(
                state
                    .conversations
                    .iter()
                    .filter(|c| c.id == second)
                    .count(),
                1
            );
            assert_eq!(state.resume_in_pane("%1", &report(second)), None);
            assert_eq!(state.resume_in_pane("%2", &report(first)), None);

            let restored: State =
                serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            assert_eq!(
                restored.conversation(second).unwrap().pane_id.as_deref(),
                Some("%1")
            );
            assert_eq!(
                restored.conversation(first).unwrap().cwd,
                Path::new("/work/b")
            );
        }
    }
}
