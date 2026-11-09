//! The `docs` toolset: pack validation, pack fixes, Bedrock JSON schemas and Molang
//! evaluation. None of these need the game to run.

use serde_json::json;

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, object},
};
use crate::docs::{check, knowledge, mct, molang, schemas, script_api};

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
                        "- Some checks download vanilla data.\n",
                        "- pack_check finds broken references between files, with their line and column.\n\n",
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
            "Check pack references",
            |_, a| reply(check::run(&arguments(a)?)?),
            json!({
                "name": "pack_check",
                "description": format!(
                    concat!(
                        "Use this to find broken references between pack files without the game, after you add or rename a texture, function, loot table, item, block, entity, model, animation or sound.\n\n",
                        "Reads every .json and .mcfunction file of the behavior and resource packs in path and checks that each name a file refers to exists in the add-on or in vanilla. ",
                        "Vanilla names come from {repository} {pinned}, downloaded once into the cache with curl. Nothing runs and nothing is changed.\n\n",
                        "- Finds packs by their manifest.json, up to four folders deep; a .mcpack, .mcaddon, .mcworld or .zip is read in place.\n",
                        "- Reads comments and trailing commas as the game does; a file it cannot parse is a json-syntax error.\n",
                        "- Rules: texture-missing, loot-table-missing, trade-table-missing, function-missing, item-missing, block-missing, entity-missing, geometry-missing, ",
                        "render-controller-missing, animation-missing, sound-missing, particle-missing, manifest-dependency-missing, json-syntax.\n",
                        "- Commands are read in .mcfunction files, tick.json, run_command and queue_command events, and the / lines of behavior animations and controllers: ",
                        "function, summon, give, setblock, fill, playsound and particle.\n",
                        "- References into a pack type path lacks (textures without a resource pack) are skipped and named in notes.\n",
                        "- Without vanilla data, names that may be vanilla are warnings, not errors.\n",
                        "- Does not check schema fields or Molang; pack_validate and the content log cover those.\n\n",
                        "Returns {{source: \"references\", ok, diagnostics: [{{file, line, column, severity, rule, message, source}}], counts, files_scanned, packs: [{{label, kind, name, folder}}], vanilla, truncated?, notes}}: ",
                        "file starts with its pack's label (BP or RP), ok is false when there are errors, vanilla names the identifier data used or is null, ",
                        "truncated means only the first 1000 diagnostics are listed."
                    ),
                    repository = schemas::REPOSITORY,
                    pinned = schemas::PINNED
                ),
                "inputSchema": object(json!({ "path": path }), &["path"])
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
            |_, a| reply(schemas::list(&arguments(a)?)?),
            json!({
                "name": "schema_list",
                "description": format!(
                    concat!(
                        "Use this to find which schema describes a pack file type or component before you write one.\n\n",
                        "Lists the pack file types that {repository} {pinned} describes; with query, only matching documents plus matching components. ",
                        "The release's source archive is downloaded once with curl into the cache.\n\n",
                        "- Components are the namespaced keys (minecraft:health) each type's newest format_version schema reaches.\n",
                        "- Fails when curl is missing or the download fails and nothing is cached; the error names the URL.\n\n",
                        "Returns {{source: \"rockide\", version, documents: [{{id, title, pack, file_match, format_versions, component_count, description}}], ",
                        "components?: [{{kind, component}}], truncated?, notes}}: file_match globs are relative to the pack folder, ",
                        "format_versions are the ones with their own schema, oldest first."
                    ),
                    repository = schemas::REPOSITORY,
                    pinned = schemas::PINNED
                ),
                "inputSchema": object(json!({ "query": { "type": "string", "description": "Text to find, ignoring case, in document ids, titles, descriptions and component names, e.g. health (default: every document)" } }), &[])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Get a Bedrock schema",
            |_, a| reply(schemas::get(&arguments(a)?)?),
            json!({
                "name": "schema_get",
                "description": concat!(
                    "Use this to read the exact fields a pack file or component takes.\n\n",
                    "Reads one JSON schema from the release schema_list uses: give id, or kind with an optional component. ",
                    "A file type's schema is the one for its newest format_version unless format_version picks another.\n\n",
                    "- A component is the first property of that name the type's schema reaches through its $refs; when it is a $ref, the schema it points to.\n",
                    "- Fails when nothing matches, kind is ambiguous or format_version is unknown; the error says where to look.\n\n",
                    "Returns {source: \"rockide\", version, id, schema, refs, kind?, component?, format_version?, notes}: ",
                    "id and refs are schema ids (a file, with #/pointer for a part of one) to pass back as id."
                ),
                "inputSchema": object(json!({
                    "id": { "type": "string", "description": "A document id from schema_list (bp/entities) or a schema id from refs (bedrock/server/entity/legacy/components/health); not with kind" },
                    "kind": { "type": "string", "description": "A document id, the same without its pack (entities, attachables), its title or its schema folder (server/entity); not with id" },
                    "component": { "type": "string", "description": "A component of kind, with or without the minecraft: prefix (minecraft:health or health)" },
                    "format_version": { "type": "string", "description": "The file's format_version, e.g. 1.21.0, with kind or a document id (default: the newest in format_versions)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Script API reference",
            |s, a| reply(script_api::lookup(s.install(), &arguments(a)?)?),
            json!({
                "name": "script_api",
                "description": concat!(
                    "Use this to look up a Script API class, method, property or event before you write or fix add-on scripts.\n\n",
                    "Reads Mojang's script module metadata from the bedrock-samples release that matches the installed game and joins it with the JSDoc of the npm package for the same module version. ",
                    "symbol gives one declaration in full; query lists matching names. Both are downloaded once into the cache.\n\n",
                    "- The release follows the installed game (a hotfix uses its release line); when the game version is unknown or has no release, it uses the newest stable release and says so in notes.\n",
                    "- since is the oldest stable version of the module's major version that has the symbol; beta_only means only the beta module has it, which needs the Beta APIs experiment.\n",
                    "- An unknown symbol returns found: false with close matches. Without npm the reply has no descriptions and a note says why.\n",
                    "- Fails when curl is missing or the download fails and nothing is cached; the error names the URL.\n\n",
                    "Returns {source: {repository, release, game_version, docs}, module, module_version, channel, found, symbol, kind, signature, description, ",
                    "parameters: [{name, type, optional, default, min, max, description}], returns: {type, description}, throws, privileges, since, beta_only?, members?, examples?, matches?, notes}: ",
                    "throws names the error classes; privileges are the execution modes allowed to call it (default alone means not in read-only or early execution)."
                ),
                "inputSchema": object(json!({
                    "symbol": { "type": "string", "description": "A declaration: Entity, Entity.applyDamage, world.afterEvents.entityDie or system.runTimeout; not with query" },
                    "query": { "type": "string", "description": "Text to find in symbol names, ignoring case, e.g. entityDie; not with symbol" },
                    "module": { "type": "string", "description": "The script module, with or without @minecraft/ (default: @minecraft/server)" },
                    "version": { "type": "string", "description": "stable (default: the newest stable version in the release), beta, or an exact module version such as 2.4.0" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Component reference",
            |s, a| reply(knowledge::component(s.install(), &arguments(a)?)?),
            json!({
                "name": "component_docs",
                "description": concat!(
                    "Use this to learn what a component's properties mean, with their types and defaults, before you write it into a pack file.\n\n",
                    "Reads the component's schema from Mojang's JSON schema metadata in the bedrock-samples release that matches the installed game: each property's type, default, range and description. ",
                    "It also gives the rockide schema id for schema_get and the vanilla files that use the component.\n\n",
                    "- Searches every kind unless kind is given; a component of several kinds, such as minecraft:tags, needs kind.\n",
                    "- When Mojang's metadata gives a component no properties (minecraft:health), they come from the rockide schema; properties_source and a note say so.\n",
                    "- An unknown component returns found: false with close names.\n\n",
                    "Returns {source, found, kind, component, description, format_version, forms, properties: [{name, type, default, description, minimum, maximum, values, fields}], ",
                    "required?, schema_file, rockide_schema, vanilla_examples: {count, files}, properties_source?, matches?, notes}: forms are the value shapes the component accepts (boolean, object, ...); ",
                    "fields are the keys of an object property; schema_file and vanilla files are paths in the release."
                ),
                "inputSchema": object(json!({
                    "component": { "type": "string", "description": "The component, with or without the minecraft: prefix (minecraft:behavior.tempt or health)" },
                    "kind": { "type": "string", "enum": knowledge::kind_names(), "description": "The file type the component belongs to (default: every kind)" }
                }), &["component"])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Command reference",
            |s, a| reply(knowledge::command(s.install(), &arguments(a)?)?),
            json!({
                "name": "command_docs",
                "description": concat!(
                    "Use this to check a slash command's syntax before you run it or put it in a function file.\n\n",
                    "Reads the command list in Mojang's metadata from the bedrock-samples release that matches the installed game: each overload's parameters, their types and the values of enum parameters.\n\n",
                    "- Takes aliases and a leading slash; an unknown name returns found: false with commands whose name or description match.\n",
                    "- Enum parameters with more than 60 values (every item or block id) give value_count instead of values.\n\n",
                    "Returns {source, found, name, description, aliases, permission_level, requires_cheats, overloads: [{usage, params: [{name, type, optional, values?, value_count?}]}], matches?, notes}: ",
                    "usage puts required parameters in <> and optional ones in []."
                ),
                "inputSchema": object(json!({ "name": { "type": "string", "description": "The command, e.g. give or /gamemode" } }), &["name"])
            }),
        ),
        tool(
            Hints::READ.open_world(),
            "Molang reference",
            |s, a| reply(knowledge::molang(s.install(), &arguments(a)?)?),
            json!({
                "name": "molang_docs",
                "description": concat!(
                    "Use this to check what a Molang query or math function returns and which arguments it takes.\n\n",
                    "Reads the Molang query and math function list in Mojang's metadata from the bedrock-samples release that matches the installed game.\n\n",
                    "- Takes query.x, q.x, math.x or a bare name; an unknown name returns found: false with entries whose name or description match.\n\n",
                    "Returns {source, found, kind, name, description, return_type, min_args, max_args, version_ranges, matches?, notes}: ",
                    "version_ranges give the first (and last) game version a query works in."
                ),
                "inputSchema": object(json!({ "name": { "type": "string", "description": "The query or function, e.g. query.is_sneaking, q.health or math.lerp" } }), &["name"])
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
