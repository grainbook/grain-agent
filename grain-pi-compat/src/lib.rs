//! `grain-pi-compat` — run pi-style extensions on the grain runtime.
//!
//! [pi extensions](https://pi.dev/docs/latest/extensions) are TypeScript
//! / JavaScript files exporting a factory:
//!
//! ```js
//! // <workspace>/.pi/extensions/my-tool.js
//! export default (pi) => {
//!   pi.registerTool({
//!     name: "shout",
//!     description: "Uppercases text",
//!     parameters: { type: "object", properties: { text: { type: "string" }}},
//!     execute: (args) => args.text.toUpperCase(),
//!   });
//! };
//! ```
//!
//! This crate adapts that shape to our [`grain_script_boa::BoaExtension`]
//! by source-transforming each file: prepending a small shim that
//! aliases pi's camelCase API onto `grain`'s snake_case API, then —
//! when an `export default` factory entry is present — wrapping the
//! call to feed it the `pi` object. The transformed file is written
//! to a temp dir, then `BoaExtension::from_scripts_dir` loads it.
//!
//! ## What works in Phase 1
//! - `pi.registerTool({ name, description, parameters, execute })`
//! - `export default (pi) => {...}` factory entry
//! - Top-level `pi.registerTool(...)` (no factory) — also fine
//! - Discovery: `<workspace>/.pi/extensions/*.js` +
//!   `~/.pi/agent/extensions/*.js`
//!
//! ## Not yet (Phase 2+)
//! - `pi.on(event, handler)` event subscriptions
//! - `pi.registerCommand` / `pi.registerShortcut`
//! - `ctx.ui.*` interactive prompts
//! - TypeScript source via swc
//! - npm package extensions

pub mod extension;
pub mod transform;

pub use extension::{PiCommand, PiCompatError, PiExtension, PiNotification, PiShortcut};
pub use transform::transform_pi_source;
