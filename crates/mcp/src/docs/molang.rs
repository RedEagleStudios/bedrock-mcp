//! Molang evaluation with npm `molangjs`, run by Node.js or Bun. molangjs evaluates
//! leniently: it does not check names or syntax, so a result is never proof of validity.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::Path,
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ran, Scratch, ensure_package, run, which};
use crate::error::{Error, Result, bail};

pub const PACKAGE: &str = "molangjs";
pub const PINNED: &str = "1.7.1";
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STEPS: u32 = 1000;

/// Always part of the reply: what a molangjs result does and does not show.
pub const LENIENT_NOTE: &str = "molangjs evaluates leniently: unknown names become 0 and malformed syntax is not reported, so this result is not proof the expression is valid. The game reports Molang errors in the content log when it loads the pack; read them with content_log_read.";

/// Reads `{module, expression, use_radians, runs: [{variables}]}` from stdin and prints one
/// `{value}` or `{error}` per run. A fresh parser per run keeps `v.` assignments apart.
const RUNNER: &str = r#"import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const request = JSON.parse(readFileSync(0, 'utf8'));
const { default: Molang } = await import(pathToFileURL(request.module).href);
const results = request.runs.map(({ variables }) => {
  const molang = new Molang();
  molang.use_radians = request.use_radians;
  try {
    const value = molang.parse(request.expression, variables);
    return typeof value === 'number' && !Number.isFinite(value) ? { value: String(value) } : { value };
  } catch (error) {
    return { error: String(error && error.message ? error.message : error) };
  }
});
process.stdout.write(JSON.stringify(results));
"#;

/// molangjs's own `math.` functions (1.7.1) and the `query.` functions it implements.
const BUILTINS: &[&str] = &[
    "math.pi",
    "math.abs",
    "math.sin",
    "math.cos",
    "math.exp",
    "math.ln",
    "math.pow",
    "math.sqrt",
    "math.random",
    "math.ceil",
    "math.round",
    "math.trunc",
    "math.floor",
    "math.mod",
    "math.min",
    "math.max",
    "math.clamp",
    "math.lerp",
    "math.lerprotate",
    "math.asin",
    "math.acos",
    "math.atan",
    "math.atan2",
    "math.die_roll",
    "math.die_roll_integer",
    "math.hermite_blend",
    "math.random_integer",
    "math.min_angle",
    "math.sign",
    "math.copy_sign",
    "math.inverse_lerp",
    "math.ease_in_quad",
    "math.ease_out_quad",
    "math.ease_in_out_quad",
    "math.ease_in_cubic",
    "math.ease_out_cubic",
    "math.ease_in_out_cubic",
    "math.ease_in_quart",
    "math.ease_out_quart",
    "math.ease_in_out_quart",
    "math.ease_in_quint",
    "math.ease_out_quint",
    "math.ease_in_out_quint",
    "math.ease_in_sine",
    "math.ease_out_sine",
    "math.ease_in_out_sine",
    "math.ease_in_expo",
    "math.ease_out_expo",
    "math.ease_in_out_expo",
    "math.ease_in_circ",
    "math.ease_out_circ",
    "math.ease_in_out_circ",
    "math.ease_in_bounce",
    "math.ease_out_bounce",
    "math.ease_in_out_bounce",
    "math.ease_in_back",
    "math.ease_out_back",
    "math.ease_in_out_back",
    "math.ease_in_elastic",
    "math.ease_out_elastic",
    "math.ease_in_out_elastic",
    "query.in_range",
    "query.all",
    "query.any",
    "query.approx_eq",
];

const PREFIXES: &[&str] = &[
    "query", "q", "math", "variable", "v", "temp", "t", "context", "c",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
    pub name: String,
    pub from: f64,
    pub to: f64,
    pub steps: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eval {
    pub expression: String,
    #[serde(default)]
    pub queries: BTreeMap<String, Value>,
    #[serde(default)]
    pub sweep: Option<Sweep>,
    #[serde(default)]
    pub use_radians: bool,
}

/// A name as molangjs looks it up: lower case, with `q.`/`v.`/`t.`/`c.` spelled out.
pub fn normalize(name: &str) -> String {
    let name = name.trim().to_lowercase();
    match name.split_once('.') {
        Some(("q", rest)) => format!("query.{rest}"),
        Some(("v", rest)) => format!("variable.{rest}"),
        Some(("t", rest)) => format!("temp.{rest}"),
        Some(("c", rest)) => format!("context.{rest}"),
        _ => name,
    }
}

/// The sweep's input values: `steps` evenly spaced values from `from` to `to`, inclusive.
fn sweep_inputs(sweep: &Sweep) -> Result<Vec<f64>> {
    if !(2..=MAX_STEPS).contains(&sweep.steps) {
        bail!("sweep.steps is the number of values, from 2 to {MAX_STEPS}.");
    }
    if !sweep.from.is_finite() || !sweep.to.is_finite() {
        bail!("sweep.from and sweep.to must be finite numbers.");
    }
    let last = f64::from(sweep.steps - 1);
    Ok((0..sweep.steps)
        .map(|step| sweep.from + (sweep.to - sweep.from) * f64::from(step) / last)
        .collect())
}

/// One evaluation: its sweep input, if any, and its variables keyed as molangjs reads them.
pub type Run = (Option<f64>, BTreeMap<String, Value>);

/// The variables of each run, keyed as molangjs looks them up, and each run's sweep input.
pub fn runs(request: &Eval) -> Result<Vec<Run>> {
    if request.expression.trim().is_empty() {
        bail!("expression is empty.");
    }
    let mut base = BTreeMap::new();
    for (name, value) in &request.queries {
        if !(value.is_number() || value.is_string()) {
            bail!("queries[{name:?}] must be a number or a Molang expression string.");
        }
        base.insert(normalize(name), value.clone());
    }
    let Some(sweep) = &request.sweep else {
        return Ok(vec![(None, base)]);
    };
    let key = normalize(&sweep.name);
    Ok(sweep_inputs(sweep)?
        .into_iter()
        .map(|input| {
            let mut variables = base.clone();
            variables.insert(key.clone(), json!(input));
            (Some(input), variables)
        })
        .collect())
}

/// The names with a known prefix that `expression` reads but neither assigns, the caller
/// supplied, nor molangjs implements; molangjs evaluated each of them as 0.
pub fn unsupplied(expression: &str, supplied: &BTreeSet<String>) -> Vec<String> {
    let text = expression.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut reads: Vec<(String, String)> = Vec::new();
    let mut assigned = BTreeSet::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\'' {
            at += text[at + 1..].find('\'').map_or(bytes.len(), |end| end + 2);
            continue;
        }
        if !(byte.is_ascii_alphabetic() || byte == b'_')
            || (at > 0 && (word(bytes[at - 1]) || bytes[at - 1] == b'.'))
        {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len()
            && (word(bytes[at])
                || (bytes[at] == b'.' && bytes.get(at + 1).copied().is_some_and(word)))
        {
            at += 1;
        }
        let name = &text[start..at];
        let Some((prefix, _)) = name.split_once('.') else {
            continue;
        };
        if !PREFIXES.contains(&prefix) {
            continue;
        }
        let long = normalize(name);
        let rest = text[at..].trim_start();
        if rest.starts_with('=') && !rest.starts_with("==") {
            assigned.insert(long);
        } else {
            reads.push((long, expression[start..at].to_owned()));
        }
    }
    let mut seen = BTreeSet::new();
    reads
        .into_iter()
        .filter(|(long, _)| {
            !supplied.contains(long)
                && !assigned.contains(long)
                && !BUILTINS.contains(&long.as_str())
                && seen.insert(long.clone())
        })
        .map(|(_, written)| written)
        .collect()
}

/// The runner's output: one `{value}` or `{error}` per run.
pub fn parse_results(ran: &Ran, count: usize) -> Result<Vec<Value>> {
    if ran.code != Some(0) {
        bail!(
            "The Molang runner failed (exit {:?}): {}",
            ran.code,
            ran.stderr_tail()
        );
    }
    let results: Vec<Value> = serde_json::from_str(ran.stdout.trim()).map_err(|error| {
        Error::msg(format!(
            "The Molang runner printed no result list ({error}): {}",
            ran.stdout.trim()
        ))
    })?;
    if results.len() != count
        || results
            .iter()
            .any(|result| result.get("value").is_none() && result.get("error").is_none())
    {
        bail!(
            "The Molang runner answered {} results for {count} runs: {}",
            results.len(),
            ran.stdout.trim()
        );
    }
    Ok(results)
}

/// Node.js on PATH, else Bun.
fn runtime() -> Result<std::path::PathBuf> {
    which("node").or_else(|| which("bun")).ok_or_else(|| {
        Error::msg("molang_eval needs Node.js or Bun on PATH to run molangjs; neither was found.")
    })
}

pub fn eval(request: &Eval) -> Result<Value> {
    let runs = runs(request)?;
    let program = runtime()?;
    let package = ensure_package(PACKAGE, PINNED)?;
    let module = package.join("dist").join("molang.esm.js");
    if !module.is_file() {
        bail!("{} is missing from the molangjs package.", module.display());
    }
    let scratch = Scratch::new("molang")?;
    let script = scratch.path().join("molang.mjs");
    fs::write(&script, RUNNER)?;
    let input = json!({
        "module": module,
        "expression": request.expression,
        "use_radians": request.use_radians,
        "runs": runs.iter().map(|(_, variables)| json!({ "variables": variables })).collect::<Vec<_>>(),
    });
    let ran = run(
        &program,
        &[OsString::from(&script)],
        scratch.path(),
        Some(&serde_json::to_vec(&input)?),
        TIMEOUT,
    )?;
    let results = parse_results(&ran, runs.len())?;
    reply(request, &runs, results, &program)
}

fn reply(request: &Eval, runs: &[Run], results: Vec<Value>, program: &Path) -> Result<Value> {
    let mut supplied: BTreeSet<String> =
        request.queries.keys().map(|name| normalize(name)).collect();
    if let Some(sweep) = &request.sweep {
        supplied.insert(normalize(&sweep.name));
    }
    let runtime = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned());
    let mut reply = json!({
        "source": "molangjs",
        "unsupplied": unsupplied(&request.expression, &supplied),
        "molangjs_version": PINNED,
        "runtime": runtime,
        "notes": [LENIENT_NOTE],
    });
    if request.sweep.is_some() {
        let values: Vec<Value> = runs
            .iter()
            .zip(results)
            .map(|((input, _), mut result)| {
                result["input"] = json!(input);
                result
            })
            .collect();
        reply["values"] = values.into();
    } else {
        let result = &results[0];
        if let Some(error) = result["error"].as_str() {
            bail!("molangjs could not evaluate the expression: {error}");
        }
        reply["value"] = result["value"].clone();
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Eval {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn short_names_are_spelled_out_as_molangjs_reads_them() {
        assert_eq!(normalize("q.anim_time"), "query.anim_time");
        assert_eq!(normalize("V.Speed"), "variable.speed");
        assert_eq!(normalize("t.i"), "temp.i");
        assert_eq!(normalize("c.other"), "context.other");
        assert_eq!(normalize("query.x"), "query.x");
        assert_eq!(normalize("math.pi"), "math.pi");
    }

    #[test]
    fn queries_and_sweeps_become_one_variable_set_per_run() {
        let runs = runs(&request(json!({
            "expression": "q.anim_time * v.x",
            "queries": { "v.x": "1+2", "query.life_time": 4 },
            "sweep": { "name": "q.anim_time", "from": 0, "to": 1, "steps": 3 }
        })))
        .unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(
            runs.iter()
                .map(|(input, _)| input.unwrap())
                .collect::<Vec<_>>(),
            [0.0, 0.5, 1.0]
        );
        assert_eq!(
            json!(runs[1].1),
            json!({ "query.anim_time": 0.5, "query.life_time": 4, "variable.x": "1+2" })
        );
        let single = super::runs(&request(json!({ "expression": "1" }))).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].0, None);

        let bad = |value: Value| super::runs(&request(value)).unwrap_err().to_string();
        assert!(bad(json!({ "expression": " " })).contains("empty"));
        assert!(
            bad(json!({ "expression": "1", "queries": { "v.x": [1] } }))
                .contains("number or a Molang expression")
        );
        assert!(bad(json!({ "expression": "1", "sweep": { "name": "q.x", "from": 0, "to": 1, "steps": 1 } })).contains("from 2"));
        assert!(
            serde_json::from_value::<Eval>(json!({ "expression": "1", "strict": true })).is_err()
        );
    }

    #[test]
    fn unsupplied_lists_names_that_evaluated_as_zero() {
        let supplied: BTreeSet<String> = ["query.anim_time".to_owned()].into();
        assert_eq!(
            unsupplied(
                "v.x = 3; t.y = v.x * q.Anim_Time + query.not_a_real_query + math.sinn(10) + math.sin(v.z) + Q.anim_time; return t.y == c.w;",
                &supplied
            ),
            ["query.not_a_real_query", "math.sinn", "v.z", "c.w"]
        );
        // String literals, plain numbers and other identifiers are not names.
        assert!(
            unsupplied(
                "query.is_item_name_any('slot.weapon.mainhand', 0, 'x:y') + 1.5",
                &BTreeSet::new()
            ) == ["query.is_item_name_any"]
        );
        assert!(unsupplied("math.pi * query.in_range(1, 0, 2)", &BTreeSet::new()).is_empty());
    }

    fn ran(code: i32, stdout: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.into(),
            stderr: "SyntaxError: boom".into(),
        }
    }

    #[test]
    fn runner_output_is_checked_and_shaped_into_the_reply() {
        let results = parse_results(
            &ran(0, r#"[{"value":1},{"value":"NaN"},{"error":"bad"}]"#),
            3,
        )
        .unwrap();
        let eval = request(json!({
            "expression": "q.anim_time + q.nope",
            "sweep": { "name": "q.anim_time", "from": 0, "to": 2, "steps": 3 }
        }));
        let runs = runs(&eval).unwrap();
        let out = reply(&eval, &runs, results, Path::new("/usr/bin/bun")).unwrap();
        assert_eq!(out["source"], "molangjs");
        assert_eq!(out["runtime"], "bun");
        assert_eq!(out["molangjs_version"], PINNED);
        assert_eq!(out["unsupplied"], json!(["q.nope"]));
        assert_eq!(
            out["values"],
            json!([{ "input": 0.0, "value": 1 }, { "input": 1.0, "value": "NaN" }, { "input": 2.0, "error": "bad" }])
        );
        assert!(out.get("value").is_none());
        assert!(
            out["notes"][0]
                .as_str()
                .unwrap()
                .contains("content_log_read")
        );

        let single = request(json!({ "expression": "1 +* 2" }));
        let out = reply(
            &single,
            &super::runs(&single).unwrap(),
            vec![json!({ "value": 1 })],
            Path::new("node"),
        )
        .unwrap();
        assert_eq!(out["value"], 1);
        assert!(out["notes"][0].as_str().unwrap().contains("not proof"));
        assert!(
            reply(
                &single,
                &super::runs(&single).unwrap(),
                vec![json!({ "error": "x" })],
                Path::new("node")
            )
            .is_err()
        );

        assert!(
            parse_results(&ran(1, ""), 1)
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        assert!(parse_results(&ran(0, "oops"), 1).is_err());
        assert!(parse_results(&ran(0, r#"[{"value":1}]"#), 2).is_err());
        assert!(parse_results(&ran(0, r#"[{"other":1}]"#), 1).is_err());
    }
}
