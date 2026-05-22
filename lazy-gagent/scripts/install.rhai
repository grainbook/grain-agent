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
// `plugin-spec.toml` bootstrap that pulls this script in.

fn tools() {
    [
        #{
            name: "lazy_install",
            description: "Install a plugin: append a [[plugin]] entry to plugin-spec.toml and sync (git clone for URLs, symlink for local paths). `src` accepts the same forms as the spec file: https://..., git@..., or a filesystem path. Returns a status message.",
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
            description: "Drop a plugin from plugin-spec.toml. By default also tears down the installed directory (symlinks are unlinked, real dirs are rm -rf'd); pass delete_files=false to keep the local files.",
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
