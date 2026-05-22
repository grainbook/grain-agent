`lazy-gagent` is a placeholder plugin. The plugin **system** (manifest discovery, skill/theme/prompt/script integration) lives in `grain-ai-agent-headless::plugins`. This plugin's eventual job is to manage other plugins — install / update / remove — by orchestrating `grain-ai-agent-headless::sync_plugins` from inside an agent turn rather than only at boot.

Phase C-0 (`plugin-spec.toml` declarative install) is already shipped in the engine. Phase C-1 will fill in this directory with:

- `scripts/install.js` — a Boa-loaded tool the agent can call: `lazy_gagent.install(name, src)` appends a `[[plugin]]` block to `plugin-spec.toml` and runs sync.
- `scripts/update.js` — `git pull` on git-sourced plugins; refresh local symlinks (no-op, since the source tree is the live data).
- `scripts/remove.js` — drop the `[[plugin]]` block plus optionally `rm -rf` the installed directory.

Until those land, treat this plugin as documentation: when a user asks "how do I install plugin X" the answer is "edit `<workspace>/.grain/plugin-spec.toml` and add a `[[plugin]]` entry — engine syncs at next launch".
