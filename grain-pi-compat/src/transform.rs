//! Source-level transformation: pi-style JS → grain-style JS.
//!
//! The transform is two steps:
//!
//! 1. Prepend a JS *shim* that defines `pi` as an object whose methods
//!    call into the pre-existing `grain` global. Method-name + field-
//!    name translation lives here (pi uses `parameters` + `execute`,
//!    grain uses `schema` + `run`).
//! 2. If the source begins with `export default ...`, strip the
//!    `export default ` keyword and wrap the resulting expression in
//!    `(...)(pi);` so the factory is invoked with our `pi` shim.
//!
//! The transform is deliberately textual + conservative. Real ES
//! module loading lands in Phase 3 alongside TypeScript support.

/// The JS shim prepended to every transformed file. Defines `pi` as
/// an object that translates pi's camelCase API onto grain's
/// snake_case API.
pub const SHIM_HEADER: &str = r#"
// --- grain-pi-compat shim (auto-generated, do not edit) ---
const pi = {
  registerTool: function (opts) {
    if (!opts || typeof opts !== 'object') {
      throw new Error("pi.registerTool: argument must be an object");
    }
    grain.register_tool({
      name: opts.name,
      description: opts.description,
      // pi calls it `parameters`; grain calls it `schema`.
      schema: opts.parameters,
      // pi calls it `execute`; grain calls it `run`.
      run: opts.execute,
    });
  },
  // Subscribe to an agent lifecycle event. The host bridges the
  // matching `AgentEvent` variant into a JSON payload and dispatches
  // here. Supported event names (Phase 2):
  //   "agent_start" | "agent_end" | "tool_call" | "tool_result"
  //   | "message_start" | "message_end"
  // Unsupported names (pi has them; we don't yet): session_*,
  // before_agent_start, input — subscribing is silently a no-op.
  on: function (event_name, handler) {
    if (typeof event_name !== 'string' || !event_name) {
      throw new Error("pi.on: event_name must be a non-empty string");
    }
    if (typeof handler !== 'function') {
      throw new Error("pi.on: handler must be a function");
    }
    grain.register_callback("on:" + event_name, handler);
  },
  // Register a slash command. pi.dev signature:
  //   pi.registerCommand(name, { description, handler })
  // The handler is wired through grain.register_callback under
  // `cmd:<name>`; the description lands in grain.register_meta so
  // higher layers (the TUI's slash palette) can render it.
  registerCommand: function (name, opts) {
    if (typeof name !== 'string' || !name) {
      throw new Error("pi.registerCommand: name (string) is required");
    }
    if (!opts || typeof opts !== 'object') {
      throw new Error("pi.registerCommand: opts must be an object");
    }
    if (typeof opts.handler !== 'function') {
      throw new Error("pi.registerCommand: opts.handler must be a function");
    }
    grain.register_meta("command", name, {
      description: opts.description || "",
    });
    grain.register_callback("cmd:" + name, opts.handler);
  },
  // Register a keyboard shortcut. pi.dev signature:
  //   pi.registerShortcut(keys, { description?, handler })
  // `keys` is a free-form spec like "ctrl+x" or "shift+alt+a";
  // it's stored verbatim. The TUI is responsible for parsing the
  // spec and matching incoming key events against it.
  registerShortcut: function (keys, opts) {
    if (typeof keys !== 'string' || !keys) {
      throw new Error("pi.registerShortcut: keys (string) is required");
    }
    if (!opts || typeof opts !== 'object') {
      throw new Error("pi.registerShortcut: opts must be an object");
    }
    if (typeof opts.handler !== 'function') {
      throw new Error("pi.registerShortcut: opts.handler must be a function");
    }
    grain.register_meta("shortcut", keys, {
      description: opts.description || "",
    });
    grain.register_callback("shortcut:" + keys, opts.handler);
  },
  // pi's `ctx.ui.*` lives under `pi.ui` here — we don't thread the
  // `ctx` parameter into handlers in Phase 5a; `pi.ui.notify(...)`
  // is callable both at module top-level and from inside handlers.
  // Phase 5b adds the `ctx` argument.
  ui: {
    // Fire-and-forget toast. Host drains the queue on its own
    // cadence (e.g. once per TUI render tick) and renders each
    // payload however it wants.
    notify: function (text) {
      grain.push_notification({
        kind: "notify",
        text: typeof text === 'string' ? text : String(text),
      });
    },
    // Synchronous yes/no modal. Blocks the worker until the host
    // calls `PiExtension::resolve_modal(request_id, true|false)`.
    // Returns the boolean directly to JS.
    confirm: function (prompt) {
      return grain.modal_request("confirm", {
        prompt: typeof prompt === 'string' ? prompt : String(prompt),
      });
    },
    // Synchronous single-line text input. Returns the string the
    // host resolves with (or empty string on cancel — host's
    // choice).
    input: function (prompt) {
      return grain.modal_request("input", {
        prompt: typeof prompt === 'string' ? prompt : String(prompt),
      });
    },
    // Synchronous pick-from-list. `items` is an array of strings;
    // host resolves with the chosen string.
    select: function (prompt, items) {
      if (!Array.isArray(items)) {
        throw new Error("pi.ui.select: items must be an array");
      }
      return grain.modal_request("select", {
        prompt: typeof prompt === 'string' ? prompt : String(prompt),
        items: items.map(String),
      });
    },
  },
};
// --- end shim ---
"#;

/// Transform one pi extension source into a grain-loadable script.
///
/// Two recognized entry shapes:
///
/// 1. **Top-level**: the source already calls `pi.registerTool(...)`
///    at top level. We just prepend the shim.
/// 2. **Factory**: the source starts with `export default <expr>`
///    where `<expr>` evaluates to a function `(pi) => {...}`. We
///    strip the keyword, parenthesize the expression, and apply it
///    to our `pi` shim.
pub fn transform_pi_source(source: &str) -> String {
    let trimmed = source.trim_start();
    if let Some(rest) = trimmed.strip_prefix("export default") {
        // The factory body — may end with `;` or not.
        let body = rest.trim().trim_end_matches(';').trim();
        return format!("{SHIM_HEADER}\n({body})(pi);\n");
    }
    format!("{SHIM_HEADER}\n{source}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_call_is_left_in_place_after_shim() {
        let src = r#"pi.registerTool({ name: "x", description: "x", parameters: {}, execute: () => "" });"#;
        let out = transform_pi_source(src);
        assert!(out.contains("const pi = {"));
        assert!(out.contains("pi.registerTool({ name: \"x\""));
        // shim is before user code:
        let shim_pos = out.find("const pi = {").unwrap();
        let usr_pos = out.find("pi.registerTool({ name").unwrap();
        assert!(shim_pos < usr_pos);
    }

    #[test]
    fn factory_entry_gets_unwrapped_and_invoked() {
        let src = "export default (pi) => { pi.registerTool({ name: \"f\" }); };";
        let out = transform_pi_source(src);
        // The factory is now a call expression rather than an
        // `export default` statement.
        assert!(!out.contains("export default"));
        assert!(out.contains("(pi) => { pi.registerTool({ name: \"f\" }); })(pi);"));
    }

    #[test]
    fn factory_without_trailing_semicolon_still_works() {
        let src = "export default (pi) => { pi.registerTool({ name: \"g\" }); }";
        let out = transform_pi_source(src);
        assert!(out.contains("})(pi);"));
    }
}
