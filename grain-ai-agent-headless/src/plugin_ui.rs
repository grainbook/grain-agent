//! UI extension primitives for plugins.
//!
//! A plugin can extend an existing TUI overlay (today: `/plugins`)
//! by declaring `[[ui_command]]` entries in its `plugin.toml` and
//! implementing handler functions in its Rhai scripts. The TUI shows
//! the registered keys as footer hints; when the user presses a key,
//! the TUI invokes the named Rhai handler and renders the returned
//! [`OverlayDescriptor`].
//!
//! The engine itself only owns the **data shape** here. The TUI owns
//! the actual widget rendering, and `grain-script-rhai` owns the
//! handler dispatch. That keeps the plugin model declarative on the
//! manifest side, scriptable on the behavior side, and the engine
//! UI-agnostic.

use serde::{Deserialize, Serialize};

use crate::plugins::Plugin;

/// One declarative UI extension entry from `plugin.toml`.
///
/// ```toml
/// [[ui_command]]
/// target  = "plugins"
/// key     = "i"
/// label   = "Install"
/// handler = "ui_install_prompt"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiCommand {
    /// Built-in overlay this entry extends. Today only `"plugins"`
    /// is honored by the TUI; future overlays will accept their own
    /// keys (e.g. `"skills"`, `"providers"`).
    pub target: String,
    /// Single-character key shown in the overlay footer. Routed to
    /// `handler` when pressed inside the matching overlay.
    pub key: String,
    /// Footer label rendered next to `key`, e.g. `"Install"`.
    pub label: String,
    /// Rhai function name to invoke. Defined in one of the plugin's
    /// `scripts/*.rhai` files. Receives no arguments; must return a
    /// map matching one of the [`OverlayDescriptor`] variants.
    pub handler: String,
}

/// Severity hint for [`OverlayDescriptor::Modal`]. Picks the accent
/// color in the TUI.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModalSeverity {
    #[default]
    Info,
    Success,
    Warn,
    Error,
}

/// One input field inside an [`OverlayDescriptor::Form`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormField {
    /// Field key — used in the `data` map handed to the submit
    /// handler.
    pub name: String,
    /// Human-readable label shown above the input.
    pub label: String,
    /// Optional placeholder shown when the input is empty.
    #[serde(default)]
    pub placeholder: String,
    /// Optional pre-filled value.
    #[serde(default)]
    pub initial: String,
}

/// What a Rhai UI handler returns: a description of the widget the
/// TUI should render. The TUI knows how to render each variant; new
/// widget kinds require an engine + TUI release in lockstep, which
/// is exactly the contract we want for security.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayDescriptor {
    /// Multi-field form. User edits each field, presses Enter to
    /// submit — `on_submit` Rhai function gets called with a map
    /// keyed by `FormField::name`.
    Form {
        title: String,
        fields: Vec<FormField>,
        on_submit: String,
    },
    /// One-shot message box, dismissed with Esc / Enter. No handler;
    /// terminal state of a flow.
    Modal {
        title: String,
        body: String,
        #[serde(default)]
        severity: ModalSeverity,
    },
    /// Yes / no prompt. `on_yes` runs with `yes_args` on confirm; No
    /// just dismisses.
    Confirm {
        title: String,
        body: String,
        on_yes: String,
        #[serde(default)]
        yes_args: serde_json::Value,
    },
}

impl OverlayDescriptor {
    /// Title to render in the overlay's top bar.
    pub fn title(&self) -> &str {
        match self {
            Self::Form { title, .. } | Self::Modal { title, .. } | Self::Confirm { title, .. } => {
                title
            }
        }
    }
}

/// A [`UiCommand`] paired with the plugin that contributed it. The
/// TUI uses `plugin_name` for footer attribution ("Install [i]
/// — lazy-gagent") and for "which script defines this handler"
/// lookups in `grain-script-rhai`-land.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoundUiCommand {
    pub plugin_name: String,
    pub command: UiCommand,
}

/// Collect every `[[ui_command]]` block declared by `plugins`, paired
/// with its source plugin name. Stable order: alphabetical by
/// plugin name (matches [`crate::plugins::discover_plugins`]'s sort),
/// then declaration order within each manifest.
pub fn collect_ui_commands(plugins: &[Plugin]) -> Vec<BoundUiCommand> {
    let mut out = Vec::new();
    for p in plugins {
        for cmd in &p.manifest.ui_commands {
            out.push(BoundUiCommand {
                plugin_name: p.manifest.name.clone(),
                command: cmd.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_command_deserializes_from_toml_block() {
        let raw = r#"
            target = "plugins"
            key = "i"
            label = "Install"
            handler = "ui_install_prompt"
        "#;
        let cmd: UiCommand = toml::from_str(raw).unwrap();
        assert_eq!(cmd.target, "plugins");
        assert_eq!(cmd.key, "i");
        assert_eq!(cmd.label, "Install");
        assert_eq!(cmd.handler, "ui_install_prompt");
    }

    #[test]
    fn overlay_descriptor_form_roundtrips_json() {
        let d = OverlayDescriptor::Form {
            title: "Install plugin".into(),
            fields: vec![FormField {
                name: "name".into(),
                label: "Name".into(),
                placeholder: "lazy-gagent".into(),
                initial: String::new(),
            }],
            on_submit: "ui_install_submit".into(),
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains("\"kind\":\"form\""));
        let back: OverlayDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn overlay_descriptor_modal_severity_defaults_info() {
        let raw = r#"{"kind":"modal","title":"Done","body":"ok"}"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::Modal { severity, .. } => {
                assert_eq!(severity, ModalSeverity::Info)
            }
            _ => panic!("expected modal"),
        }
    }

    #[test]
    fn overlay_descriptor_confirm_yes_args_optional() {
        let raw = r#"{"kind":"confirm","title":"Remove?","body":"sure","on_yes":"do_remove"}"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::Confirm { yes_args, .. } => {
                assert!(yes_args.is_null());
            }
            _ => panic!("expected confirm"),
        }
    }

    #[test]
    fn title_helper_returns_each_variant() {
        let f = OverlayDescriptor::Form {
            title: "f".into(),
            fields: vec![],
            on_submit: "x".into(),
        };
        let m = OverlayDescriptor::Modal {
            title: "m".into(),
            body: String::new(),
            severity: ModalSeverity::Info,
        };
        let c = OverlayDescriptor::Confirm {
            title: "c".into(),
            body: String::new(),
            on_yes: "x".into(),
            yes_args: serde_json::Value::Null,
        };
        assert_eq!(f.title(), "f");
        assert_eq!(m.title(), "m");
        assert_eq!(c.title(), "c");
    }
}
