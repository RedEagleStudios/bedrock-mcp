//! `cargo run --release -p rtti --example anchors -- <exe>...`
//!
//! Resolves every anchor rule in each executable. Where the executable has
//! RTTI for a class, it checks that anchors found the same vtable; elsewhere
//! it reports whether exactly one vtable matched. Exits non-zero if a rule
//! disagrees with RTTI or matched more than one vtable.

use std::collections::HashMap;

use rtti::anchors::{Outcome, RULES, resolve};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        return Err("usage: anchors <exe>...".into());
    }
    let mut failed = false;
    for path in &paths {
        let bytes = std::fs::read(path)?;
        let image = rtti::Image::from_file(&bytes)?;
        let start = std::time::Instant::now();
        let outcomes = resolve(&image, &RULES);
        let took = start.elapsed();
        let classes = rtti::index(&image);
        let names: HashMap<u32, String> = classes
            .values()
            .flatten()
            .map(|table| (table.rva, format!("{}+{:#x}", table.name, table.offset)))
            .collect();
        let name = |rva: &u32| names.get(rva).map_or("no RTTI", String::as_str).to_owned();
        println!(
            "{path}: anchors resolved in {took:?}; {} classes with RTTI",
            classes.len()
        );
        for (rule, outcome) in RULES.iter().zip(&outcomes) {
            let truth = classes
                .get(&rtti::decorated(rule.class, false))
                .and_then(|tables| tables.iter().find(|table| table.offset == rule.offset));
            let label = format!("{} +{:#x}", rule.class, rule.offset);
            let verdict = match (outcome, truth) {
                (Outcome::Found(found), Some(truth)) if found.rva == truth.rva => {
                    "matched RTTI".to_owned()
                }
                (Outcome::Found(found), Some(truth)) => {
                    failed = true;
                    format!(
                        "MISMATCH: anchors {:#x} ({}), RTTI {:#x} ({} slots)",
                        found.rva,
                        name(&found.rva),
                        truth.rva,
                        truth.slots.len()
                    )
                }
                (Outcome::Found(found), None) => format!("unique at {:#x}", found.rva),
                (Outcome::Family(family), Some(truth))
                    if family.iter().any(|table| table.rva == truth.rva) =>
                {
                    format!("matched RTTI within a family of {}", family.len())
                }
                (Outcome::Family(family), truth) => {
                    let members: Vec<String> = family
                        .iter()
                        .map(|table| format!("{:#x} ({})", table.rva, name(&table.rva)))
                        .collect();
                    if truth.is_some() {
                        failed = true;
                        format!("MISMATCH: family {} lacks RTTI's", members.join(", "))
                    } else {
                        format!("unique family {}", members.join(", "))
                    }
                }
                (Outcome::Ambiguous(starts), _) => {
                    failed = true;
                    let starts: Vec<String> = starts
                        .iter()
                        .map(|start| format!("{start:#x} ({})", name(start)))
                        .collect();
                    format!("AMBIGUOUS: {}", starts.join(", "))
                }
                (Outcome::Missing(reason), Some(truth)) => format!(
                    "missing ({reason}); RTTI has it at {:#x} with {} slots",
                    truth.rva,
                    truth.slots.len()
                ),
                (Outcome::Missing(reason), None) => format!("missing ({reason})"),
            };
            println!("  {label:<32} {verdict}");
        }
    }
    if failed {
        return Err("anchors disagree with RTTI or are ambiguous".into());
    }
    Ok(())
}
