use std::{
    fs,
    io::{self, Write},
};

use super::*;
use crate::testdir::TestDir;

const BP_UUID: &str = "11111111-1111-4111-8111-111111111111";
const RP_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// A small add-on in which every reference resolves, through the add-on or the fake vanilla
/// lists of [`vanilla`].
fn clean() -> Vec<(&'static str, String)> {
    let files: &[(&str, &str)] = &[
        (
            "BP/manifest.json",
            r#"{
  "format_version": 2,
  "header": { "name": "Demo BP", "uuid": "BP_UUID", "version": [1, 0, 0] },
  "modules": [{ "type": "data", "uuid": "33333333-3333-4333-8333-333333333333", "version": [1, 0, 0] }],
  "dependencies": [
    { "uuid": "RP_UUID", "version": [1, 0, 0] },
    { "module_name": "@minecraft/server", "version": "1.0.0" }
  ]
}"#,
        ),
        (
            "RP/manifest.json",
            r#"{
  "format_version": 2,
  "header": { "name": "Demo RP", "uuid": "RP_UUID", "version": [1, 0, 0] },
  "modules": [{ "type": "resources", "uuid": "44444444-4444-4444-8444-444444444444", "version": [1, 0, 0] }]
}"#,
        ),
        (
            "BP/items/gem.json",
            r#"{
  // Comments and trailing commas, as the game accepts them.
  "minecraft:item": {
    "description": { "identifier": "demo:gem", },
    "components": {
      "minecraft:icon": { "textures": { "default": "demo_gem" } },
      "minecraft:block_placer": { "block": "demo:ore" },
      /* a block comment */
    },
  },
}"#,
        ),
        (
            "BP/blocks/ore.json",
            r#"{
  "minecraft:block": {
    "description": { "identifier": "demo:ore" },
    "components": {
      "minecraft:material_instances": { "*": { "texture": "demo_ore" } },
      "minecraft:geometry": "geometry.demo_ore",
      "minecraft:loot": "loot_tables/blocks/ore.json"
    }
  }
}"#,
        ),
        (
            "BP/entities/bob.json",
            r#"{
  "minecraft:entity": {
    "description": {
      "identifier": "demo:bob",
      "animations": { "tick": "controller.animation.bob.tick" }
    },
    "component_groups": {
      "demo:drops": { "minecraft:loot": { "table": "loot_tables/entities/bob.json" } }
    },
    "components": {
      "minecraft:spawn_entity": { "entities": [{ "spawn_item": "demo:gem" }] }
    },
    "events": {
      "demo:start": { "queue_command": { "command": ["function demo/start"] } }
    }
  }
}"#,
        ),
        (
            "BP/animation_controllers/bob.json",
            r#"{
  "animation_controllers": {
    "controller.animation.bob.tick": {
      "states": { "default": { "on_entry": ["/function demo/start", "@s demo:start"] } }
    }
  }
}"#,
        ),
        (
            "BP/loot_tables/blocks/ore.json",
            r#"{ "pools": [{ "entries": [
  { "type": "item", "name": "demo:gem" },
  { "type": "loot_table", "name": "loot_tables/entities/bob" }
] }] }"#,
        ),
        (
            "BP/loot_tables/entities/bob.json",
            r#"{ "pools": [{ "entries": [{ "type": "item", "name": "minecraft:stick" }] }] }"#,
        ),
        (
            "BP/recipes/gem.json",
            r##"{
  "minecraft:recipe_shaped": {
    "pattern": ["#"],
    "key": { "#": { "item": "minecraft:stick" } },
    "result": { "item": "demo:gem" }
  }
}"##,
        ),
        (
            "BP/spawn_rules/bob.json",
            r#"{
  "minecraft:spawn_rules": {
    "description": { "identifier": "demo:bob" },
    "conditions": [{ "minecraft:spawns_on_block_filter": "minecraft:stone" }]
  }
}"#,
        ),
        ("BP/functions/tick.json", r#"{ "values": ["demo/start"] }"#),
        (
            "BP/functions/demo/start.mcfunction",
            "# Start the demo\nsummon demo:bob ~ ~ ~\nexecute as @a run give @s demo:gem\nplaysound demo.boom @a\n",
        ),
        (
            "RP/textures/terrain_texture.json",
            r#"{ "texture_data": { "demo_ore": { "textures": "textures/blocks/demo_ore" } } }"#,
        ),
        (
            "RP/textures/item_texture.json",
            r#"{ "texture_data": { "demo_gem": { "textures": ["textures/items/demo_gem"] } } }"#,
        ),
        ("RP/textures/blocks/demo_ore.png", ""),
        ("RP/textures/items/demo_gem.png", ""),
        ("RP/textures/entity/bob.tga", ""),
        ("RP/sounds/demo/boom.ogg", ""),
        (
            "RP/models/entity/bob.geo.json",
            r#"{ "minecraft:geometry": [
  { "description": { "identifier": "geometry.bob" } },
  { "description": { "identifier": "geometry.demo_ore" } }
] }"#,
        ),
        (
            "RP/entity/bob.entity.json",
            r#"{
  "minecraft:client_entity": {
    "description": {
      "identifier": "demo:bob",
      "textures": { "default": "textures/entity/bob" },
      "geometry": { "default": "geometry.bob" },
      "render_controllers": ["controller.render.bob", { "controller.render.default": "true" }],
      "animations": { "walk": "animation.bob.walk" }
    }
  }
}"#,
        ),
        (
            "RP/render_controllers/bob.json",
            r#"{ "render_controllers": { "controller.render.bob": {} } }"#,
        ),
        (
            "RP/animations/bob.json",
            r#"{ "animations": { "animation.bob.walk": {} } }"#,
        ),
        (
            "RP/sounds/sound_definitions.json",
            r#"{ "format_version": "1.14.0", "sound_definitions": { "demo.boom": { "sounds": [{ "name": "sounds/demo/boom" }] } } }"#,
        ),
        (
            "RP/sounds.json",
            r#"{ "individual_event_sounds": { "events": { "boom": { "sound": "demo.boom" } } } }"#,
        ),
    ];
    files
        .iter()
        .map(|(path, text)| {
            (
                *path,
                text.replace("BP_UUID", BP_UUID).replace("RP_UUID", RP_UUID),
            )
        })
        .collect()
}

fn vanilla() -> Vanilla {
    Vanilla::from_lists(&[
        (Kind::Item, &["minecraft:stick", "minecraft:stone"]),
        (Kind::Block, &["minecraft:stone"]),
        (Kind::Entity, &["minecraft:zombie"]),
        (Kind::RenderController, &["controller.render.default"]),
    ])
}

fn tree(files: &[(&str, String)]) -> Tree {
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (*path, text.as_str()))
        .collect();
    Tree::from_files(&files)
}

/// `files` with `from` replaced by `to` in the file at `path`.
fn edit(files: &mut [(&str, String)], path: &str, from: &str, to: &str) {
    let (_, text) = files.iter_mut().find(|(file, _)| *file == path).unwrap();
    assert!(text.contains(from), "{path} lacks {from}");
    *text = text.replacen(from, to, 1);
}

/// The line and column (in characters) where `needle` first starts in `path`.
fn position(files: &[(&str, String)], path: &str, needle: &str) -> (u32, u32) {
    let (_, text) = files.iter().find(|(file, _)| *file == path).unwrap();
    let offset = text.find(needle).unwrap();
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap().chars().count() + 1;
    (line as u32, column as u32)
}

type Found = (String, Option<(u32, u32)>, &'static str, Severity);

fn found(report: &Report) -> Vec<Found> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let at = diagnostic.line.zip(diagnostic.column);
            (
                diagnostic.file.clone(),
                at,
                diagnostic.rule,
                diagnostic.severity,
            )
        })
        .collect()
}

#[test]
fn a_clean_addon_has_no_diagnostics() {
    let report = check(&tree(&clean()), Some(&vanilla()));
    assert_eq!(found(&report), []);
    assert_eq!(report.packs.len(), 2);
    assert_eq!(
        (
            report.packs[0].label.as_str(),
            report.packs[1].label.as_str()
        ),
        ("BP", "RP")
    );
    // 19 JSON files and one function; images and sounds are only listed.
    assert_eq!(report.files_scanned, 20);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

#[test]
fn each_broken_reference_is_reported_where_it_is() {
    let mut files = clean();
    // One broken reference per rule, each replacing a good one with a quoted name.
    let breaks: &[(&str, &str, &str, &str)] = &[
        (
            "BP/blocks/ore.json",
            "\"demo_ore\"",
            "\"demo_ore_typo\"",
            "texture-missing",
        ),
        (
            "RP/textures/item_texture.json",
            "\"textures/items/demo_gem\"",
            "\"textures/items/gem\"",
            "texture-missing",
        ),
        (
            "BP/entities/bob.json",
            "\"loot_tables/entities/bob.json\"",
            "\"loot_tables/entities/nope.json\"",
            "loot-table-missing",
        ),
        (
            "BP/functions/tick.json",
            "\"demo/start\"",
            "\"demo/stop\"",
            "function-missing",
        ),
        (
            "BP/recipes/gem.json",
            "\"demo:gem\"",
            "\"demo:jewel\"",
            "item-missing",
        ),
        (
            "BP/items/gem.json",
            "\"demo:ore\"",
            "\"demo:rock\"",
            "block-missing",
        ),
        (
            "BP/spawn_rules/bob.json",
            "\"demo:bob\"",
            "\"demo:bobby\"",
            "entity-missing",
        ),
        (
            "BP/blocks/ore.json",
            "\"geometry.demo_ore\"",
            "\"geometry.ore\"",
            "geometry-missing",
        ),
        (
            "RP/entity/bob.entity.json",
            "\"controller.render.bob\"",
            "\"controller.render.robert\"",
            "render-controller-missing",
        ),
        (
            "RP/entity/bob.entity.json",
            "\"animation.bob.walk\"",
            "\"animation.bob.run\"",
            "animation-missing",
        ),
        (
            "RP/sounds.json",
            "\"demo.boom\"",
            "\"demo.bang\"",
            "sound-missing",
        ),
    ];
    let mut expected = Vec::new();
    for &(path, from, to, rule) in breaks {
        edit(&mut files, path, from, to);
        expected.push((
            path.to_owned(),
            Some(position(&files, path, to)),
            rule,
            Severity::Error,
        ));
    }
    // A dependency on a pack that is not here may be installed apart: a warning.
    let other = "\"55555555-5555-4555-8555-555555555555\"";
    edit(
        &mut files,
        "BP/manifest.json",
        &format!("\"{RP_UUID}\", \"version\""),
        &format!("{other}, \"version\""),
    );
    let at = position(&files, "BP/manifest.json", other);
    expected.push((
        "BP/manifest.json".into(),
        Some(at),
        "manifest-dependency-missing",
        Severity::Warning,
    ));
    // A command in a function file is located by its own line and column.
    edit(
        &mut files,
        "BP/functions/demo/start.mcfunction",
        "summon demo:bob",
        "summon demo:ghost",
    );
    expected.push((
        "BP/functions/demo/start.mcfunction".into(),
        Some((2, 8)),
        "entity-missing",
        Severity::Error,
    ));
    // A file the game cannot parse.
    files.push((
        "BP/items/broken.json",
        "{\n  \"a\": 1\n  \"b\": 2\n}".into(),
    ));
    expected.push((
        "BP/items/broken.json".into(),
        Some((3, 3)),
        "json-syntax",
        Severity::Error,
    ));

    let report = check(&tree(&files), Some(&vanilla()));
    expected.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    assert_eq!(found(&report), expected);
    assert_eq!(report.count(Severity::Error), expected.len() - 1);
}

#[test]
fn messages_name_the_reference_and_where_it_was_looked_for() {
    let mut files = clean();
    edit(
        &mut files,
        "BP/functions/tick.json",
        "demo/start",
        "demo/stop",
    );
    let report = check(&tree(&files), Some(&vanilla()));
    assert_eq!(
        report.diagnostics[0].to_string(),
        "BP/functions/tick.json:1:14: error function-missing: function \"demo/stop\" is not defined (looked in BP functions/ and vanilla)"
    );
    assert_eq!(report.diagnostics[0].to_json()["source"], "references");
}

#[test]
fn vanilla_names_resolve_and_unknown_ones_depend_on_the_data() {
    let mut files = clean();
    edit(
        &mut files,
        "BP/recipes/gem.json",
        "\"result\": { \"item\": \"demo:gem\" }",
        "\"result\": [{ \"item\": \"minecraft:stone\" }, \"minecraft:stik\", \"demo:nope\"]",
    );
    let at = |needle| Some(position(&files, "BP/recipes/gem.json", needle));
    let file = || "BP/recipes/gem.json".to_owned();

    // With vanilla data: stone resolves; a minecraft: name vanilla lacks may be a legacy
    // alias, so it is a warning; another namespace is an error.
    let report = check(&tree(&files), Some(&vanilla()));
    assert_eq!(
        found(&report),
        [
            (
                file(),
                at("\"minecraft:stik\""),
                "item-missing",
                Severity::Warning
            ),
            (file(), at("\"demo:nope\""), "item-missing", Severity::Error),
        ]
    );

    // Without it, every minecraft: name is a warning and the notes say why.
    let report = check(&tree(&files), None);
    let recipe: Vec<_> = found(&report)
        .into_iter()
        .filter(|found| found.0 == file())
        .collect();
    assert_eq!(recipe.len(), 4);
    assert_eq!(
        recipe
            .iter()
            .filter(|found| found.3 == Severity::Error)
            .count(),
        1
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("Vanilla data was unavailable"))
    );
}

#[test]
fn names_are_normalized_like_the_game_reads_them() {
    assert_eq!(Kind::Item.normalize("stick"), "minecraft:stick");
    assert_eq!(Kind::Item.normalize("minecraft:dye:4"), "minecraft:dye");
    assert_eq!(Kind::Item.normalize("bucket:1"), "minecraft:bucket");
    assert_eq!(
        Kind::Entity.normalize("minecraft:zombie<minecraft:as_baby>"),
        "minecraft:zombie"
    );
    assert_eq!(
        Kind::LootTable.normalize("loot_tables/a"),
        "loot_tables/a.json"
    );
    assert_eq!(
        Kind::Texture.normalize("textures/blocks/a.PNG"),
        "textures/blocks/a"
    );
    assert_eq!(Kind::SoundFile.normalize("sounds/a.ogg"), "sounds/a");
}

#[test]
fn references_into_a_missing_pack_are_skipped_with_a_note() {
    let files: Vec<_> = clean()
        .into_iter()
        .filter(|(path, _)| path.starts_with("BP/"))
        .collect();
    let report = check(&tree(&files), Some(&vanilla()));
    // Only the dependency on the absent resource pack is reported.
    let rules: Vec<_> = found(&report).into_iter().map(|found| found.2).collect();
    assert_eq!(rules, ["manifest-dependency-missing"]);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("No resource pack"))
    );
}

#[test]
fn a_dependency_on_another_version_is_an_error() {
    let mut files = clean();
    edit(
        &mut files,
        "RP/manifest.json",
        "\"version\": [1, 0, 0] }",
        "\"version\": [2, 0, 0] }",
    );
    let report = check(&tree(&files), Some(&vanilla()));
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].severity, Severity::Error);
    assert!(
        report.diagnostics[0].message.contains("\"Demo RP\" 1.0.0"),
        "{}",
        report.diagnostics[0].message
    );
}

#[test]
fn several_packs_of_a_kind_are_labelled_by_folder() {
    let mut files = clean();
    let second = files
        .iter()
        .find(|(path, _)| *path == "BP/manifest.json")
        .unwrap()
        .1
        .replace(BP_UUID, "66666666-6666-4666-8666-666666666666");
    files.push(("packs/extra/manifest.json", second));
    files.push((
        "packs/extra/functions/tick.json",
        r#"{ "values": ["demo/start", "demo/gone"] }"#.into(),
    ));
    let report = check(&tree(&files), Some(&vanilla()));
    let labels: Vec<_> = report
        .packs
        .iter()
        .map(|pack| pack.label.as_str())
        .collect();
    assert_eq!(labels, ["BP/BP", "RP", "BP/packs/extra"]);
    // Packs of an add-on share their names: demo/start resolves through the first pack.
    let found = found(&report);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "BP/packs/extra/functions/tick.json");
}

#[test]
fn reads_packs_inside_an_mcaddon() {
    let dir = TestDir::new("check-archive");
    let pack = |files: &[(&str, String)], prefix: &str| {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (path, text) in files.iter().filter(|(path, _)| path.starts_with(prefix)) {
            writer
                .start_file(
                    &path[prefix.len()..],
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    };
    let mut files = clean();
    edit(
        &mut files,
        "BP/functions/tick.json",
        "demo/start",
        "demo/stop",
    );
    let addon = dir.path().join("demo.mcaddon");
    let mut writer = zip::ZipWriter::new(fs::File::create(&addon).unwrap());
    for (name, prefix) in [("Demo BP.mcpack", "BP/"), ("Demo RP.mcpack", "RP/")] {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&pack(&files, prefix)).unwrap();
    }
    writer.finish().unwrap();

    let report = check(&Tree::load(&addon).unwrap(), Some(&vanilla()));
    let found = found(&report);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "BP/functions/tick.json");
    assert_eq!(report.packs[0].folder, "Demo BP.mcpack/");
}

#[test]
fn reads_a_folder_and_skips_hidden_ones() {
    let dir = TestDir::new("check-folder");
    for (path, text) in clean() {
        let path = dir.path().join("project").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    // A tool's copy of the pack in a hidden folder is not a second pack.
    let copy = dir.path().join("project/.regolith/BP");
    fs::create_dir_all(&copy).unwrap();
    fs::write(copy.join("manifest.json"), "{ broken").unwrap();

    let report = check(
        &Tree::load(&dir.path().join("project")).unwrap(),
        Some(&vanilla()),
    );
    assert_eq!(found(&report), []);
    assert_eq!(report.packs.len(), 2);
    assert!(Tree::load(&dir.path().join("missing")).is_err());
}
