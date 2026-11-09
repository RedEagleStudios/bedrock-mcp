//! Minecraft Creator Tools (`mct`, npm `@minecraft/creator-tools`): pack validation, project
//! information and version fixes, run as a subprocess.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Ran, Scratch, copy_dir, json_files, node_version, run, which};
use crate::error::{Error, Result, bail};

pub const PACKAGE: &str = "@minecraft/creator-tools";
/// The release `npx` runs when no `mct` is installed.
pub const PINNED: &str = "0.21.0";
/// Path to an `mct` executable to use instead of the one on PATH.
pub const SETTING: &str = "BEDROCK_MCP_MCT";
/// Creator Tools declares no engine, but its CLI is tested on Node.js 22 and newer.
const NODE_MAJOR: u32 = 22;
/// The first `npx` run downloads about 180 MB.
const TIMEOUT: Duration = Duration::from_secs(300);

/// How `mct` is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launcher {
    /// The executable named by [`SETTING`].
    Configured(PathBuf),
    /// `mct` on PATH.
    Installed(PathBuf),
    /// `npx -y @minecraft/creator-tools@PINNED`.
    Npx(PathBuf),
}

impl Launcher {
    /// [`SETTING`], else `mct` on PATH, else `npx`; the last two need Node.js 22 or newer.
    pub fn locate() -> Result<Self> {
        if let Some(path) = env::var_os(SETTING).filter(|value| !value.is_empty()) {
            return Ok(Self::Configured(path.into()));
        }
        let launcher = match (which("mct"), which("npx")) {
            (Some(mct), _) => Self::Installed(mct),
            (None, Some(npx)) => Self::Npx(npx),
            (None, None) => bail!(
                "Minecraft Creator Tools needs Node.js {NODE_MAJOR} or newer: neither mct nor npx is on PATH. Install Node.js {NODE_MAJOR}+ (npx then fetches {PACKAGE}@{PINNED}), run `npm install -g {PACKAGE}`, or set {SETTING} to an mct executable."
            ),
        };
        check_node()?;
        Ok(launcher)
    }

    /// The program and arguments that run `mct args`.
    pub fn command(&self, args: Vec<OsString>) -> (PathBuf, Vec<OsString>) {
        match self {
            Self::Configured(mct) | Self::Installed(mct) => (mct.clone(), args),
            Self::Npx(npx) => {
                let mut full = vec![
                    OsString::from("-y"),
                    OsString::from(format!("{PACKAGE}@{PINNED}")),
                ];
                full.extend(args);
                (npx.clone(), full)
            }
        }
    }

    /// The Creator Tools release this launcher runs, asked once per executable.
    pub fn version(&self) -> Option<String> {
        static SEEN: LazyLock<Mutex<HashMap<PathBuf, String>>> = LazyLock::new(Mutex::default);
        let mct = match self {
            Self::Npx(_) => return Some(PINNED.to_owned()),
            Self::Configured(mct) | Self::Installed(mct) => mct,
        };
        let mut seen = SEEN.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(version) = seen.get(mct) {
            return Some(version.clone());
        }
        let ran = run(
            mct,
            &[OsString::from("--version")],
            &env::temp_dir(),
            None,
            Duration::from_secs(60),
        )
        .ok()?;
        let version = ran.stdout.lines().next()?.trim().to_owned();
        (ran.code == Some(0) && !version.is_empty()).then(|| {
            seen.insert(mct.clone(), version.clone());
            version
        })
    }

    fn run(&self, args: Vec<OsString>, cwd: &Path) -> Result<Ran> {
        let (program, args) = self.command(args);
        run(&program, &args, cwd, None, TIMEOUT)
    }
}

/// Fail unless `node` on PATH is Node.js 22 or newer.
fn check_node() -> Result<()> {
    let node = which("node").ok_or_else(|| {
        Error::msg(format!(
            "Minecraft Creator Tools needs Node.js {NODE_MAJOR} or newer, and node is not on PATH. Install Node.js {NODE_MAJOR}+ or set {SETTING} to an mct executable."
        ))
    })?;
    let (major, text) = node_version(&node)?;
    node_gate(major, &text)
}

fn node_gate(major: u32, text: &str) -> Result<()> {
    if major < NODE_MAJOR {
        bail!(
            "Minecraft Creator Tools needs Node.js {NODE_MAJOR} or newer; node on PATH is {text}. Install Node.js {NODE_MAJOR}+ or set {SETTING} to an mct executable."
        );
    }
    Ok(())
}

/// Which validators `pack_validate` runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    /// The common checks.
    #[default]
    Main,
    /// Add-on packaging and structure checks.
    Addon,
    /// Every validator.
    All,
}

impl Suite {
    fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Addon => "addon",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validate {
    pub path: PathBuf,
    #[serde(default)]
    pub suite: Suite,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub warn_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Info {
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fix {
    /// `header.min_engine_version` in manifests.
    MinEngineVersion,
    /// `format_version` in definition files.
    FormatVersions,
}

impl Fix {
    fn name(self) -> &'static str {
        match self {
            Self::MinEngineVersion => "setnewestminengineversion",
            Self::FormatVersions => "setnewestformatversions",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixVersions {
    pub path: PathBuf,
    pub fix: Fix,
    #[serde(default = "yes")]
    pub dry_run: bool,
}

fn yes() -> bool {
    true
}

/// `-i folder` for a pack folder, `--if file` for a .mcpack/.mcaddon/.mcworld/.zip.
fn input_args(path: &Path, is_file: bool) -> Vec<OsString> {
    let flag = if is_file { "--if" } else { "-i" };
    vec![flag.into(), path.into()]
}

pub fn validate_args(request: &Validate, is_file: bool, output: &Path) -> Result<Vec<OsString>> {
    if let Some(bad) = request.exclude.iter().find(|rule| {
        rule.is_empty() || !rule.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }) {
        bail!("exclude takes rule ids such as PACKSIZE or CPACKICON; {bad:?} is not one.");
    }
    let mut args: Vec<OsString> = vec!["validate".into(), request.suite.name().into()];
    if !request.exclude.is_empty() {
        args.push(request.exclude.join(",").to_ascii_uppercase().into());
    }
    args.extend(input_args(&request.path, is_file));
    args.extend(["--json".into(), "-o".into(), output.into()]);
    if request.warn_only {
        args.push("--warn-only".into());
    }
    Ok(args)
}

pub fn info_args(path: &Path, is_file: bool) -> Vec<OsString> {
    let mut args = vec![OsString::from("info")];
    args.extend(input_args(path, is_file));
    args.push("--json".into());
    args
}

pub fn fix_args(path: &Path, fix: Fix) -> Vec<OsString> {
    let mut args = vec![OsString::from("fix"), fix.name().into()];
    args.extend(input_args(path, false));
    args.push("--json".into());
    args
}

/// The pack path as a checked absolute path, and whether it is a file.
fn pack_path(path: &Path) -> Result<(PathBuf, bool)> {
    let absolute = std::path::absolute(path)?;
    if absolute.is_dir() {
        Ok((absolute, false))
    } else if absolute.is_file() {
        Ok((absolute, true))
    } else {
        bail!("{} does not exist.", absolute.display())
    }
}

/// The JSON document in `stdout`: all of it, else from the first line that opens an object.
fn json_document(stdout: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim()).ok().or_else(|| {
        let start = stdout
            .lines()
            .scan(0, |offset, line| {
                let at = *offset;
                *offset += line.len() + 1;
                Some((at, line))
            })
            .find(|(_, line)| line.trim_start().starts_with('{'))?
            .0;
        serde_json::from_str(stdout[start..].trim()).ok()
    })
}

fn severity(kind: &str) -> Option<&'static str> {
    match kind {
        "error" | "testFail" => Some("error"),
        "warning" => Some("warning"),
        "recommendation" => Some("recommendation"),
        "internalProcessingError" => Some("internal_error"),
        _ => None,
    }
}

/// The problems in one MCT project report. A failed check (`testFail`) only summarises the
/// errors its rule already listed, so it is kept only for rules that listed none.
fn project_findings(project: &Value) -> Vec<Value> {
    let items = project["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let base = project["path"].as_str().unwrap_or("/").trim_matches('/');
    let rules_with_errors: HashSet<&str> = items
        .iter()
        .filter(|item| item["type"] == "error")
        .filter_map(|item| item["generatorId"].as_str())
        .collect();
    items
        .iter()
        .filter_map(|item| {
            let kind = item["type"].as_str()?;
            let severity = severity(kind)?;
            let rule = item["generatorId"].as_str().unwrap_or_default();
            if kind == "testFail" && rules_with_errors.contains(rule) {
                return None;
            }
            let file = item["path"].as_str().map(|path| {
                let path = path.trim_start_matches('/');
                if base.is_empty() {
                    path.to_owned()
                } else {
                    format!("{base}/{path}")
                }
            });
            let mut finding = json!({
                "severity": severity,
                "rule": rule,
                "file": file,
                "message": item["message"].as_str().unwrap_or_default(),
            });
            if let Some(data) = item.get("data") {
                finding["data"] = data.clone();
            }
            Some(finding)
        })
        .collect()
}

/// A `mct validate --json` run as the `pack_validate` result fields (without source and
/// version). Exit codes: 0 passed, 2 warnings, 3 errors, 4 failed checks, 5 internal error;
/// each still prints a report. Exit 1 means MCT could not start.
pub fn parse_validation(ran: &Ran) -> Result<Map<String, Value>> {
    let Some(code) = ran.code else {
        bail!(
            "mct validate was killed before it finished: {}",
            ran.stderr_tail()
        );
    };
    let report = json_document(&ran.stdout).filter(|report| report["projects"].is_array());
    let Some(report) = report else {
        let detail = [ran.stderr_tail(), excerpt(&ran.stdout)]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if code == 1 {
            bail!("mct validate could not start (exit 1): {detail}");
        }
        bail!("mct validate exited {code} without a JSON report: {detail}");
    };
    let findings: Vec<Value> = report["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(project_findings)
        .collect();
    let mut counts = BTreeMap::from([
        ("error", 0),
        ("warning", 0),
        ("recommendation", 0),
        ("internal_error", 0),
    ]);
    for finding in &findings {
        if let Some(count) = finding["severity"].as_str().and_then(|s| counts.get_mut(s)) {
            *count += 1;
        }
    }
    let mut notes = Vec::new();
    match code {
        0 | 2..=4 => {}
        5 => notes.push("MCT reported an internal error; some checks may not have run.".to_owned()),
        other => notes.push(format!("MCT exited with unexpected code {other}.")),
    }
    let projects: Vec<&str> = report["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|project| project["name"].as_str())
        .collect();
    let Value::Object(fields) = json!({
        "ok": code == 0,
        "exit_code": code,
        "findings": findings,
        "counts": counts,
        "projects": projects,
        "notes": notes,
    }) else {
        unreachable!()
    };
    Ok(fields)
}

fn excerpt(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(400) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_owned(),
    }
}

/// A finished non-validate command: its JSON output, or an error explaining the failure.
fn parse_command(command: &str, ran: &Ran) -> Result<Value> {
    match (ran.code, json_document(&ran.stdout)) {
        (Some(0), Some(document)) => Ok(document),
        (code, _) => {
            let detail = [ran.stderr_tail(), excerpt(&ran.stdout)]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            match code {
                Some(code) => bail!("mct {command} failed (exit {code}): {detail}"),
                None => bail!("mct {command} was killed before it finished: {detail}"),
            }
        }
    }
}

pub fn validate(request: &Validate) -> Result<Value> {
    let (path, is_file) = pack_path(&request.path)?;
    let launcher = Launcher::locate()?;
    // A fresh output folder each run: MCT reuses reports it finds in the output folder.
    let scratch = Scratch::new("mct-validate")?;
    let output = scratch.path().join("out");
    let request = Validate {
        path,
        suite: request.suite,
        exclude: request.exclude.clone(),
        warn_only: request.warn_only,
    };
    let ran = launcher.run(validate_args(&request, is_file, &output)?, scratch.path())?;
    let mut fields = parse_validation(&ran)?;
    fields.insert("source".into(), "mct".into());
    fields.insert("mct_version".into(), json!(launcher.version()));
    Ok(Value::Object(fields))
}

pub fn info(request: &Info) -> Result<Value> {
    let (path, is_file) = pack_path(&request.path)?;
    let launcher = Launcher::locate()?;
    let scratch = Scratch::new("mct-info")?;
    let ran = launcher.run(info_args(&path, is_file), scratch.path())?;
    let mut info = parse_command("info", &ran)?;
    if let Some(info) = info.as_object_mut() {
        info.remove("schemaVersion");
        info.remove("command");
    }
    let mut result = json!({ "source": "mct", "mct_version": launcher.version(), "notes": [] });
    if let (Some(result), Value::Object(info)) = (result.as_object_mut(), info) {
        result.extend(info);
    }
    Ok(result)
}

/// The fields a version fix changes: `format_version` and `header.min_engine_version`.
fn version_fields(bytes: &[u8]) -> [(&'static str, Value); 2] {
    let document: Value = serde_json::from_slice(bytes).unwrap_or_default();
    [
        ("format_version", document["format_version"].clone()),
        (
            "header.min_engine_version",
            document["header"]["min_engine_version"].clone(),
        ),
    ]
}

/// The files that differ between two snapshots, with the version fields that changed.
pub fn changed_files(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<Value> {
    after
        .iter()
        .filter(|(file, bytes)| before.get(*file) != Some(*bytes))
        .map(|(file, bytes)| {
            let old = before.get(file).map(|old| version_fields(old));
            let changes: Vec<Value> = version_fields(bytes)
                .into_iter()
                .enumerate()
                .filter_map(|(index, (field, new))| {
                    let old = old.as_ref().map_or(Value::Null, |old| old[index].1.clone());
                    (old != new).then(|| json!({ "field": field, "before": old, "after": new }))
                })
                .collect();
            json!({ "file": file, "changes": changes })
        })
        .collect()
}

pub fn fix_versions(request: &FixVersions) -> Result<Value> {
    let (path, is_file) = pack_path(&request.path)?;
    if is_file {
        bail!(
            "pack_fix_versions works on a pack folder; unpack {} first.",
            path.display()
        );
    }
    let launcher = Launcher::locate()?;
    let scratch = Scratch::new("mct-fix")?;
    // A dry run fixes a copy: MCT's own --dry-run reports nothing about what would change.
    let target = if request.dry_run {
        let copy = scratch.path().join("pack");
        copy_dir(&path, &copy)?;
        copy
    } else {
        path.clone()
    };
    let before = json_files(&target);
    let ran = launcher.run(fix_args(&target, request.fix), scratch.path())?;
    let report = parse_command("fix", &ran)?;
    let after = json_files(&target);
    let changed = changed_files(&before, &after);
    let updated: u64 = report["fixes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|fix| fix["updatedCount"].as_u64())
        .sum();
    let mut notes = vec![if request.dry_run {
        format!(
            "Dry run: the fix ran on a copy and nothing in {} changed. Pass dry_run false to apply it.",
            path.display()
        )
    } else {
        format!("The fix was applied to {}.", path.display())
    }];
    if !changed.is_empty() {
        notes.push("MCT rewrites each file it changes in its own layout (two-space indent, one array item per line).".to_owned());
    }
    Ok(json!({
        "source": "mct",
        "dry_run": request.dry_run,
        "fix": request.fix,
        "updated_count": updated,
        "changed": changed,
        "mct_version": launcher.version(),
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(code: Option<i32>, stdout: &str, stderr: &str) -> Ran {
        Ran {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Trimmed from a real `mct validate main --json` run (0.21.0) on a pack with a bad
    /// entity file and no pack icon; MCT exited 4.
    const REPORT: &str = r#"{"schemaVersion":"1.0.0","command":"validate","projects":[{"name":"broken","path":"/","items":[
        {"type":"unknown","message":"No applicable items found for test Base Game Version (BASEGAMEVER)","generatorId":"BASEGAMEVER"},
        {"type":"testPass","message":"Beta Features completed successfully","generatorId":"CBFG"},
        {"type":"testFail","message":"Found 1 error in Pack Icon check","generatorId":"CPACKICON"},
        {"type":"error","message":"pack_icon image file not found for Unnamed pack. It must use the .png extension.","generatorId":"CPACKICON"},
        {"type":"recommendation","message":"Behavior pack entity type format version (1.21.0) has a lower minor version number","path":"/entities/a.json","generatorId":"ENTITYTYPE"},
        {"type":"warning","message":"At minecraft:entity.components.minecraft:health.value, data does not match any one of the expected types","path":"/entities/a.json","generatorId":"JSONF"},
        {"type":"testFail","message":"Found 1 error in Lonely check","generatorId":"LONELY"},
        {"type":"info","message":"Behavior pack manifest min_engine_version","data":[1,21,90],"path":"/manifest.json","generatorId":"MINENGINEVER"},
        {"type":"featureAggregate","message":"File Count","data":3,"generatorId":"PACKFILECOUNT"}
    ]}],"errors":2,"warnings":1,"recommendations":1}"#;

    #[test]
    fn validation_errors_keep_the_report_and_list_only_problems() {
        let fields =
            parse_validation(&ran(Some(3), REPORT, "(node:1) ExperimentalWarning: x")).unwrap();
        assert_eq!(fields["ok"], false);
        assert_eq!(fields["exit_code"], 3);
        assert_eq!(fields["projects"], json!(["broken"]));
        assert_eq!(
            fields["findings"],
            json!([
                { "severity": "error", "rule": "CPACKICON", "file": null, "message": "pack_icon image file not found for Unnamed pack. It must use the .png extension." },
                { "severity": "recommendation", "rule": "ENTITYTYPE", "file": "entities/a.json", "message": "Behavior pack entity type format version (1.21.0) has a lower minor version number" },
                { "severity": "warning", "rule": "JSONF", "file": "entities/a.json", "message": "At minecraft:entity.components.minecraft:health.value, data does not match any one of the expected types" },
                { "severity": "error", "rule": "LONELY", "file": null, "message": "Found 1 error in Lonely check" }
            ])
        );
        assert_eq!(
            fields["counts"],
            json!({ "error": 2, "warning": 1, "recommendation": 1, "internal_error": 0 })
        );
        assert_eq!(fields["notes"], json!([]));
    }

    #[test]
    fn a_clean_run_is_ok_and_project_folders_prefix_files() {
        let report = r#"{"projects":[{"name":"addon","path":"/BP/","items":[
            {"type":"warning","message":"w","path":"/items/x.json","generatorId":"R1","data":"detail"}]}]}"#;
        let fields = parse_validation(&ran(Some(0), report, "")).unwrap();
        assert_eq!(fields["ok"], true);
        assert_eq!(fields["findings"][0]["file"], "BP/items/x.json");
        assert_eq!(fields["findings"][0]["data"], "detail");
        let fields = parse_validation(&ran(Some(5), report, "")).unwrap();
        assert!(
            fields["notes"][0]
                .as_str()
                .unwrap()
                .contains("internal error")
        );
        // Log lines before the report do not hide it.
        let noisy = format!("Validating...\n{report}");
        assert_eq!(
            parse_validation(&ran(Some(3), &noisy, "")).unwrap()["exit_code"],
            3
        );
    }

    #[test]
    fn missing_or_malformed_reports_are_errors_with_the_reason() {
        let error = parse_validation(&ran(Some(3), "{\"projects\": [", "boom")).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("exited 3 without a JSON report") && text.contains("boom"),
            "{text}"
        );
        let error = parse_validation(&ran(
            Some(1),
            "",
            "[ERROR] Input folder does not exist: '/nope/'.\n(node:1) ExperimentalWarning: localStorage",
        ))
        .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("could not start") && text.contains("does not exist"),
            "{text}"
        );
        assert!(!text.contains("ExperimentalWarning"), "{text}");
        assert!(parse_validation(&ran(None, REPORT, "")).is_err());
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn command_lines_follow_the_mct_syntax() {
        let request: Validate = serde_json::from_value(json!({
            "path": "/p/BP", "suite": "addon", "exclude": ["pathlength", "PACKSIZE"], "warn_only": true
        }))
        .unwrap();
        assert_eq!(
            strings(&validate_args(&request, false, Path::new("/tmp/o")).unwrap()),
            [
                "validate",
                "addon",
                "PATHLENGTH,PACKSIZE",
                "-i",
                "/p/BP",
                "--json",
                "-o",
                "/tmp/o",
                "--warn-only"
            ]
        );
        let request: Validate = serde_json::from_value(json!({ "path": "/p/a.mcaddon" })).unwrap();
        assert_eq!(
            strings(&validate_args(&request, true, Path::new("/o")).unwrap()),
            [
                "validate",
                "main",
                "--if",
                "/p/a.mcaddon",
                "--json",
                "-o",
                "/o"
            ]
        );
        let request: Validate =
            serde_json::from_value(json!({ "path": "/p", "exclude": ["A,B"] })).unwrap();
        assert!(validate_args(&request, false, Path::new("/o")).is_err());

        assert_eq!(
            strings(&info_args(Path::new("/p"), false)),
            ["info", "-i", "/p", "--json"]
        );
        assert_eq!(
            strings(&fix_args(Path::new("/p"), Fix::FormatVersions)),
            ["fix", "setnewestformatversions", "-i", "/p", "--json"]
        );
        let fix: FixVersions =
            serde_json::from_value(json!({ "path": "/p", "fix": "min_engine_version" })).unwrap();
        assert!(fix.dry_run);
        assert_eq!(fix_args(&fix.path, fix.fix)[1], "setnewestminengineversion");

        let npx = Launcher::Npx("/bin/npx".into());
        let (program, args) = npx.command(info_args(Path::new("/p"), false));
        assert_eq!(program, PathBuf::from("/bin/npx"));
        assert_eq!(
            strings(&args),
            [
                "-y",
                "@minecraft/creator-tools@0.21.0",
                "info",
                "-i",
                "/p",
                "--json"
            ]
        );
        assert_eq!(npx.version().as_deref(), Some(PINNED));
        let (program, args) = Launcher::Installed("/bin/mct".into()).command(vec!["info".into()]);
        assert_eq!(
            (program, strings(&args)),
            (PathBuf::from("/bin/mct"), vec!["info".to_owned()])
        );
    }

    #[test]
    fn node_older_than_22_is_refused_with_the_prerequisite() {
        assert!(node_gate(22, "v22.0.0").is_ok());
        assert!(node_gate(26, "v26.13.0").is_ok());
        let error = node_gate(20, "v20.11.1").unwrap_err().to_string();
        assert!(
            error.contains("Node.js 22 or newer") && error.contains("v20.11.1"),
            "{error}"
        );
    }

    #[test]
    fn fix_reports_changed_files_and_version_fields() {
        let before = BTreeMap::from([
            (
                "manifest.json".to_owned(),
                br#"{"format_version":2,"header":{"min_engine_version":[1,21,90]}}"#.to_vec(),
            ),
            ("texts/languages.json".to_owned(), b"[\"en_US\"]".to_vec()),
        ]);
        let mut after = before.clone();
        after.insert(
            "manifest.json".to_owned(),
            b"{\n  \"format_version\": 2,\n  \"header\": {\"min_engine_version\": [1, 26, 60]}\n}"
                .to_vec(),
        );
        assert_eq!(
            changed_files(&before, &after),
            json!([{ "file": "manifest.json", "changes": [
                { "field": "header.min_engine_version", "before": [1, 21, 90], "after": [1, 26, 60] }
            ] }])
            .as_array()
            .unwrap()
            .clone()
        );
    }
}
