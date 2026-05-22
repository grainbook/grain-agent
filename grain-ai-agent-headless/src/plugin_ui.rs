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

/// Semantic color name a `TextSpan` can request. Maps to the
/// active TUI palette so the plugin doesn't need to know the
/// user's theme; "accent" stays accent in any palette.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextColor {
    #[default]
    Fg,
    Muted,
    Accent,
    Secondary,
    Info,
    Error,
    Success,
    Warn,
}

/// One styled chunk inside a `TextLine`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextSpan {
    pub text: String,
    #[serde(default)]
    pub color: Option<TextColor>,
    #[serde(default)]
    pub bold: bool,
}

impl TextSpan {
    /// Convenience: an unstyled span. Plugins use this 90% of the time.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: None,
            bold: false,
        }
    }
}

/// One rendered row inside [`OverlayDescriptor::TextPanel`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextLine {
    #[serde(default)]
    pub spans: Vec<TextSpan>,
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
    /// Selectable list. Up/Down navigates; Enter dispatches
    /// `on_select` with `{ index, value }`. If `on_select` is
    /// `None`, Enter just dismisses the overlay.
    List {
        title: String,
        items: Vec<String>,
        #[serde(default)]
        on_select: Option<String>,
    },
    /// Tabular display with optional row selection. Up/Down navigates
    /// rows; Enter (when `on_select` is set) dispatches with
    /// `{ row_index, row: [<column>...] }`.
    Table {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        #[serde(default)]
        on_select: Option<String>,
    },
    /// Display-only multi-line styled text. The catch-all "I want
    /// to show information" widget — plugin produces the exact
    /// rows, TUI just renders. Esc dismisses.
    TextPanel {
        title: String,
        lines: Vec<TextLine>,
        #[serde(default)]
        footer: Option<String>,
    },
    /// Progress bar. `value` and `max` are integers; renderer
    /// derives the percent fill. `label` shows next to the bar.
    /// Display-only; the plugin re-issues this with updated
    /// values to animate. Esc dismisses.
    Progress {
        title: String,
        value: i64,
        max: i64,
        #[serde(default)]
        label: String,
    },
    /// Vertical container. Renders each child in declaration order
    /// with a 1-row gap. The **last child** receives key input; all
    /// earlier children are display-only chrome (titles, status
    /// rows, etc). Letting the last child be interactive covers the
    /// most common "header + interactive body" pattern without
    /// needing full focus routing across siblings.
    Stack {
        title: String,
        children: Vec<OverlayDescriptor>,
    },
}

impl OverlayDescriptor {
    /// Title to render in the overlay's top bar.
    pub fn title(&self) -> &str {
        match self {
            Self::Form { title, .. }
            | Self::Modal { title, .. }
            | Self::Confirm { title, .. }
            | Self::List { title, .. }
            | Self::Table { title, .. }
            | Self::TextPanel { title, .. }
            | Self::Progress { title, .. }
            | Self::Stack { title, .. } => title,
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
        let l = OverlayDescriptor::List {
            title: "l".into(),
            items: vec![],
            on_select: None,
        };
        let t = OverlayDescriptor::Table {
            title: "t".into(),
            columns: vec![],
            rows: vec![],
            on_select: None,
        };
        let tp = OverlayDescriptor::TextPanel {
            title: "tp".into(),
            lines: vec![],
            footer: None,
        };
        let pg = OverlayDescriptor::Progress {
            title: "pg".into(),
            value: 0,
            max: 1,
            label: String::new(),
        };
        let sk = OverlayDescriptor::Stack {
            title: "sk".into(),
            children: vec![],
        };
        assert_eq!(f.title(), "f");
        assert_eq!(m.title(), "m");
        assert_eq!(c.title(), "c");
        assert_eq!(l.title(), "l");
        assert_eq!(t.title(), "t");
        assert_eq!(tp.title(), "tp");
        assert_eq!(pg.title(), "pg");
        assert_eq!(sk.title(), "sk");
    }

    #[test]
    fn list_descriptor_round_trips_json() {
        let d = OverlayDescriptor::List {
            title: "Pick one".into(),
            items: vec!["alpha".into(), "beta".into()],
            on_select: Some("pick_handler".into()),
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains("\"kind\":\"list\""));
        let back: OverlayDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn list_on_select_is_optional() {
        let raw = r#"{"kind":"list","title":"x","items":["a"]}"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::List { on_select, .. } => assert!(on_select.is_none()),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn table_round_trips_with_rows() {
        let raw = r#"{
            "kind": "table",
            "title": "Plugins",
            "columns": ["Name", "Source"],
            "rows": [["lazy-gagent", "../lazy-gagent"], ["rust-helper", "https://x.git"]],
            "on_select": "edit_plugin"
        }"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::Table {
                columns,
                rows,
                on_select,
                ..
            } => {
                assert_eq!(columns.len(), 2);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], "lazy-gagent");
                assert_eq!(on_select.as_deref(), Some("edit_plugin"));
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn text_panel_parses_styled_spans() {
        let raw = r#"{
            "kind": "text_panel",
            "title": "Status",
            "lines": [
                { "spans": [
                    { "text": "OK ", "color": "success", "bold": true },
                    { "text": "all systems running" }
                ]}
            ]
        }"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::TextPanel { lines, .. } => {
                assert_eq!(lines.len(), 1);
                let spans = &lines[0].spans;
                assert_eq!(spans.len(), 2);
                assert_eq!(spans[0].color, Some(TextColor::Success));
                assert!(spans[0].bold);
                assert_eq!(spans[1].color, None);
                assert!(!spans[1].bold);
            }
            _ => panic!("expected text_panel"),
        }
    }

    #[test]
    fn progress_defaults_label_to_empty() {
        let raw = r#"{"kind":"progress","title":"Cloning","value":3,"max":10}"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::Progress {
                value, max, label, ..
            } => {
                assert_eq!(value, 3);
                assert_eq!(max, 10);
                assert!(label.is_empty());
            }
            _ => panic!("expected progress"),
        }
    }

    #[test]
    fn stack_nests_arbitrary_children() {
        let raw = r#"{
            "kind": "stack",
            "title": "Summary",
            "children": [
                { "kind": "text_panel", "title": "Header", "lines": [] },
                { "kind": "list", "title": "Pick", "items": ["a", "b"], "on_select": "pick" }
            ]
        }"#;
        let d: OverlayDescriptor = serde_json::from_str(raw).unwrap();
        match d {
            OverlayDescriptor::Stack { children, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], OverlayDescriptor::TextPanel { .. }));
                assert!(matches!(children[1], OverlayDescriptor::List { .. }));
            }
            _ => panic!("expected stack"),
        }
    }
}
