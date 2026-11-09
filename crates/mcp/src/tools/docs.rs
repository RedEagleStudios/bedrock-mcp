//! The `docs` toolset: pack validation, pack fixes, Bedrock JSON schemas and Molang
//! evaluation. None of these need the game to run.

use serde_json::json;

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, object},
};
use crate::docs::{mct, molang, schemas};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Docs, hints, title, run, definition)
    };
    let path = json!({ "type": "string", "description": "A pack or project folder, or a .mcpack, .mcaddon, .mcworld or .zip file" });
    vec![
        tool(
            Hints::READ.open_world(),
            "Validate a pack",
            |_, a| reply(mct::validate(&arguments(a)?)?),
            json!({
                "name": "pack_validate",
                "description": format!(
                    concat!(
                        "Use this to find a pack's errors without starting the game, before testing it or after you edit it.\n\n",
                        "Checks a behavior or resource pack, or a folder of packs, with Minecraft Creator Tools. The pack is not changed.\n\n",
                        "- Runs mct from {setting} or PATH, else npx {package}@{pinned}; needs Node.js 22 or newer.\n",
                        "- suite main runs the common checks, addon the packaging checks, all every check.\n",
                        "- exit_code is 0 for no problems, 3 errors, 4 failed checks, 5 an internal MCT error; findings are kept for each.\n",
                        "- Some checks download vanilla data.\n\n",
                        "Returns {{source: \"mct\", ok, exit_code, findings: [{{severity, rule, file, message, data?}}], counts, projects, mct_version, notes}}: ",
                        "severity is error, warning, recommendation or internal_error, rule an id such as CPACKICON, file relative to the pack."
                    ),
                    setting = mct::SETTING, package = mct::PACKAGE, pinned = mct::PINNED
                ),
                "inputSchema": object(json!({
                    "path": path,
                    "suite": { "enum": ["main", "addon", "all"], "description": "Which checks to run (default main)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Rule ids to skip, as in a finding's rule" },
                    "warn_only": { "type": "boolean", "description": "Make MCT exit 0 even with errors; findings are still reported (default false)" }
                }), &["path"])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Pack information",
            |_, a| reply(mct::info(&arguments(a)?)?),
            json!({
                "name": "pack_info",
                "description": concat!(
                    "Use this to see what a pack or project contains before you work on it.\n\n",
                    "Describes a pack or project with Minecraft Creator Tools (`mct info`): its name, behavior and resource pack folders, ",
                    "the items it contains by type and path, and error counts.\n\n",
                    "- Finds mct as pack_validate does and needs Node.js 22 or newer.\n\n",
                    "Returns MCT's fields plus source \"mct\", mct_version and notes."
                ),
                "inputSchema": object(json!({ "path": path }), &["path"])
            }),
        ),
        tool(
            Hints::SET.open_world(),
            "Update pack versions",
            |_, a| reply(mct::fix_versions(&arguments(a)?)?),
            json!({
                "name": "pack_fix_versions",
                "description": concat!(
                    "Use this when validation reports outdated min_engine_version or format_version values.\n\n",
                    "Raises a pack folder to the newest versions Minecraft Creator Tools knows. ",
                    "By default it runs the fix on a temporary copy and changes nothing; dry_run: false edits the files in path.\n\n",
                    "- MCT rewrites each file it changes in its own JSON layout.\n",
                    "- Needs Node.js 22 or newer.\n\n",
                    "Returns {source: \"mct\", dry_run, fix, updated_count, changed: [{file, changes: [{field, before, after}]}], mct_version, notes}."
                ),
                "inputSchema": object(json!({
                    "path": { "type": "string", "description": "A pack or project folder" },
                    "fix": { "enum": ["min_engine_version", "format_versions"], "description": "min_engine_version sets header.min_engine_version in manifests; format_versions sets format_version in definition files" },
                    "dry_run": { "type": "boolean", "description": "Fix a temporary copy and only report the changes (default true)" }
                }), &["path", "fix"])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "List Bedrock schemas",
            |s, a| reply(schemas::list(s.install(), &arguments(a)?)?),
            json!({
                "name": "schema_list",
                "description": format!(
                    concat!(
                        "Use this to find which schema describes a pack file type or component before you write one.\n\n",
                        "Lists the JSON file types that {package} describes; with query, only matching documents plus matching components.\n\n",
                        "- The package version follows the installed game version (from MicrosoftGame.Config) when known, else the newest.\n",
                        "- It is fetched once with npm or Bun into the cache; without npm it uses a cached version, or fails when none is cached.\n\n",
                        "Returns {{source: \"schemas\", package_version, game_version, documents: [{{id, title, pack, file_match, component_count, description}}], ",
                        "components?: [{{kind, component}}], notes}}; notes report a version mismatch or fallback."
                    ),
                    package = schemas::PACKAGE
                ),
                "inputSchema": object(json!({ "query": { "type": "string", "description": "Text to find, ignoring case, in document ids, titles, descriptions and component names, e.g. health (default: every document)" } }), &[])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Get a Bedrock schema",
            |s, a| reply(schemas::get(s.install(), &arguments(a)?)?),
            json!({
                "name": "schema_get",
                "description": concat!(
                    "Use this to read the exact fields a pack file or component takes.\n\n",
                    "Reads one JSON schema from the package schema_list uses: give id, or kind with an optional component.\n\n",
                    "- Fails when nothing matches or kind is ambiguous; the error says where to look.\n\n",
                    "Returns {source: \"schemas\", id, schema, refs, component?, kind?, package_version, game_version, notes}; refs are schema ids to pass to further calls."
                ),
                "inputSchema": object(json!({
                    "id": { "type": "string", "description": "A document id from schema_list (bp/entities) or a schema id from refs (bp/entities/entity_minecraft_health); not with kind" },
                    "kind": { "type": "string", "description": "A document id, its folder (entities, particles) or its title; not with id" },
                    "component": { "type": "string", "description": "A component of kind, with or without the minecraft: prefix (minecraft:health or health)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Evaluate Molang",
            |_, a| reply(molang::eval(&arguments(a)?)?),
            json!({
                "name": "molang_eval",
                "description": format!(
                    concat!(
                        "Use this to check a Molang expression's numbers without the game, such as an animation curve.\n\n",
                        "Evaluates a Molang expression with molangjs {pinned}, run by Node.js or Bun and fetched once into the cache.\n\n",
                        "- It does not validate: unknown names become 0 and bad syntax gives a wrong number. ",
                        "The game reports real Molang errors in the content log when it loads the pack.\n",
                        "- Trigonometry takes degrees like the game unless use_radians.\n\n",
                        "Returns {{source: \"molangjs\", value or values: [{{input, value}}], unsupplied, molangjs_version, runtime, notes}}: ",
                        "unsupplied lists the names the expression read that were neither supplied nor implemented by molangjs, so evaluated as 0."
                    ),
                    pinned = molang::PINNED
                ),
                "inputSchema": object(json!({
                    "expression": { "type": "string", "description": "The Molang expression to evaluate" },
                    "queries": { "type": "object", "additionalProperties": { "type": ["number", "string"] }, "description": "Values for names the expression reads, e.g. {\"query.anim_time\": 0.5, \"v.x\": \"math.sin(45)\"}: numbers or Molang sub-expressions" },
                    "sweep": describe(object(json!({
                        "name": { "type": "string" },
                        "from": { "type": "number" },
                        "to": { "type": "number" },
                        "steps": { "type": "integer", "minimum": 2, "maximum": 1000 }
                    }), &["name", "from", "to", "steps"]), "Evaluate once per value of name: steps evenly spaced values between from and to, both included"),
                    "use_radians": { "type": "boolean", "description": "Trigonometry takes radians instead of degrees (default false)" }
                }), &["expression"])
            }),
        ),
    ]
}
