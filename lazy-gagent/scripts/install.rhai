// lazy.gagent — plugin management tools, exposed to the agent.
//
// These wrappers turn the host-registered primitives
// (`plugins_install`, `plugins_update`, `plugins_remove`) into named
// agent tools with JSON Schema parameters. The agent can then call
// `lazy_install` / `lazy_update` / `lazy_remove` in conversation,
// just like any other tool. This is what makes lazy.gagent a real
// plugin manager rather than a placeholder.
//
// Loaded by `grain-script-rhai` when grain-ai-agent-tui is built
// with `--features scripts-rhai`. See docs/plugins.md for the
// `plugin.toml` bootstrap that pulls this script in.

fn tools() {
    [
        #{
            name: "lazy_install",
            description: "Install a plugin: append a [[plugin]] entry to plugin.toml and sync (git clone for URLs, symlink for local paths). `src` accepts the same forms as the spec file: https://..., git@..., or a filesystem path. Returns a status message.",
            parameters: #{
                type: "object",
                properties: #{
                    name: #{ type: "string", description: "Directory name under .grain/plugins/<name>/" },
                    src: #{ type: "string", description: "git URL or local filesystem path" }
                },
                required: ["name", "src"]
            },
            fn_name: "do_install"
        },
        #{
            name: "lazy_update",
            description: "Refresh an installed plugin: `git pull` for git-sourced plugins, no-op for local symlinks (source tree is live). Returns a status message.",
            parameters: #{
                type: "object",
                properties: #{
                    name: #{ type: "string", description: "Plugin name as listed in .grain/plugins/" }
                },
                required: ["name"]
            },
            fn_name: "do_update"
        },
        #{
            name: "lazy_remove",
            description: "Drop a plugin from plugin.toml. By default also tears down the installed directory (symlinks are unlinked, real dirs are rm -rf'd); pass delete_files=false to keep the local files.",
            parameters: #{
                type: "object",
                properties: #{
                    name: #{ type: "string", description: "Plugin name to remove" },
                    delete_files: #{ type: "boolean", description: "Also remove the directory (default true)" }
                },
                required: ["name"]
            },
            fn_name: "do_remove"
        }
    ]
}

fn do_install(args) {
    plugins_install(args.name, args.src)
}

fn do_update(args) {
    plugins_update(args.name)
}

fn do_remove(args) {
    let delete = if "delete_files" in args { args.delete_files } else { true };
    plugins_remove(args.name, delete)
}

// --- UI handlers --------------------------------------------------------
//
// Wired up via `[[ui_command]]` entries in plugin.toml. The
// /plugins overlay shows a footer chip per entry; pressing the
// bound key invokes the prompt handler below, which returns an
// `OverlayDescriptor` map the TUI renders as a Form / Confirm /
// Modal. Submit / yes follow-ups call the matching `_submit`
// handler, which calls the existing host primitive and returns a
// Modal describing the outcome.

fn ui_install_prompt() {
    #{
        kind: "form",
        title: "Install plugin",
        fields: [
            #{ name: "name", label: "Name (dir under .grain/plugins/<name>/)", placeholder: "my-plugin", initial: "" },
            #{ name: "src",  label: "Source (git URL or local path)",           placeholder: "https://github.com/me/foo.git", initial: "" }
        ],
        on_submit: "ui_install_submit"
    }
}

fn ui_install_submit(args) {
    let name = args.name;
    let src  = args.src;
    if name == "" || src == "" {
        return #{
            kind: "modal",
            title: "Install",
            body: "Both name and source are required.",
            severity: "error"
        };
    }
    let msg = plugins_install(name, src);
    let is_err = msg.contains("error") || msg.contains("failed");
    #{
        kind: "modal",
        title: if is_err { "Install failed" } else { "Installed" },
        body:  msg,
        severity: if is_err { "error" } else { "success" }
    }
}

fn ui_update_prompt() {
    #{
        kind: "form",
        title: "Update plugin",
        fields: [
            #{ name: "name", label: "Plugin name", placeholder: "lazy-gagent", initial: "" }
        ],
        on_submit: "ui_update_submit"
    }
}

fn ui_update_submit(args) {
    let name = args.name;
    if name == "" {
        return #{
            kind: "modal",
            title: "Update",
            body: "Name is required.",
            severity: "error"
        };
    }
    let msg = plugins_update(name);
    let is_err = msg.contains("error");
    #{
        kind: "modal",
        title: if is_err { "Update failed" } else { "Update" },
        body:  msg,
        severity: if is_err { "error" } else { "info" }
    }
}

fn ui_remove_prompt() {
    #{
        kind: "form",
        title: "Remove plugin",
        fields: [
            #{ name: "name", label: "Plugin name to remove", placeholder: "my-plugin", initial: "" }
        ],
        on_submit: "ui_remove_confirm"
    }
}

fn ui_remove_confirm(args) {
    let name = args.name;
    if name == "" {
        return #{
            kind: "modal",
            title: "Remove",
            body: "Name is required.",
            severity: "error"
        };
    }
    #{
        kind: "confirm",
        title: "Remove plugin",
        body:  "Remove '" + name + "' from plugin.toml and delete its installed files? This cannot be undone.",
        on_yes: "ui_remove_submit",
        yes_args: #{ name: name }
    }
}

fn ui_remove_submit(args) {
    let msg = plugins_remove(args.name, true);
    let is_err = msg.contains("error");
    #{
        kind: "modal",
        title: if is_err { "Remove failed" } else { "Removed" },
        body:  msg,
        severity: if is_err { "error" } else { "success" }
    }
}

// --- /plugins takeover ---------------------------------------------------
//
// Replaces the built-in `/plugins` read-only overlay with a richer
// Table view. Each row is one effective plugin (config-declared,
// lock-installed, or legacy-spec). Up/Down navigates, Enter pops
// a Modal with full details. The title carries a one-line
// breakdown of how many plugins come from each origin so the user
// can see at a glance what state the workspace is in.

fn ui_plugins_panel() {
    let plugins = plugins_list();
    let n_config = 0;
    let n_lock = 0;
    let n_legacy = 0;
    let n_manual = 0;
    for p in plugins {
        if p.origin == "config" { n_config += 1; }
        else if p.origin == "lock" { n_lock += 1; }
        else if p.origin == "legacy" { n_legacy += 1; }
        else { n_manual += 1; }
    }
    let rows = [];
    for p in plugins {
        // `p.wasm` is `true` when the plugin ships a Component-Model
        // `.wasm` module (loaded via `grain-plugin-wasm` when the
        // `wasm-plugins` feature is on). The host already validated
        // it; we just flag it here so it stands apart from
        // script-only plugins in the table.
        let kind_tag = if p.wasm { " · wasm" } else { "" };
        let stats = `${p.skills}s · ${p.themes}t · ${p.scripts}sc · ${p.prompts}p${kind_tag}`;
        let src_short = if p.src.len() > 40 { p.src.sub_string(0, 37) + "..." } else { p.src };
        rows.push([
            p.name,
            if p.version == "" { "—" } else { p.version },
            p.origin,
            src_short,
            stats
        ]);
    }
    let total = plugins.len();
    // Rhai arrays don't ship `join` — concatenate by hand. Each
    // origin chip shows up in the title only when its count > 0
    // so the summary stays compact for small workspaces.
    let summary = "";
    if n_config > 0 { summary += ` · ${n_config} config`; }
    if n_lock > 0   { summary += ` · ${n_lock} lock`; }
    if n_legacy > 0 { summary += ` · ${n_legacy} legacy`; }
    if n_manual > 0 { summary += ` · ${n_manual} manual`; }
    #{
        kind: "table",
        title: `Plugins (${total}${summary})`,
        columns: ["Name", "Version", "Origin", "Source", "Stats"],
        rows: rows,
        on_select: "ui_plugin_details"
    }
}

fn ui_plugin_details(args) {
    let plugins = plugins_list();
    if args.row_index < 0 || args.row_index >= plugins.len() {
        return #{
            kind: "modal",
            title: "Plugin details",
            body: "(row out of range)",
            severity: "error"
        };
    }
    let p = plugins[args.row_index];
    let origin_explain = if p.origin == "config" {
        "declared in config.toml (immutable from runtime)"
    } else if p.origin == "lock" {
        "added by lazy_install (writable via lazy_remove)"
    } else if p.origin == "legacy" {
        "in legacy plugin.toml (deprecated — consider migrating to config.toml)"
    } else {
        "discovered on disk only (no spec entry)"
    };
    let body = `Name:        ${p.name}
Version:     ${if p.version == "" { "—" } else { p.version }}
Author:      ${if p.author == "" { "—" } else { p.author }}
Origin:      ${p.origin}  (${origin_explain})
Source:      ${if p.src == "" { "—" } else { p.src }}
Rev:         ${if p.rev == "" { "—" } else { p.rev }}
Root:        ${p.root}

Contributions:
  Skills:    ${p.skills}
  Themes:    ${p.themes}
  Scripts:   ${p.scripts}
  Prompts:   ${p.prompts}

Description:
${if p.description == "" { "  (none)" } else { "  " + p.description }}`;
    #{
        kind: "modal",
        title: `Plugin: ${p.name}`,
        body: body,
        severity: "info"
    }
}
