//! References from sequence steps to the entities earlier `spawn` steps summoned: `$A` names
//! the spawn step labelled `as: "A"`, `$spawn:2` the spawn step at index 2. They stand for the
//! entity id in step arguments and in conditions; an event condition whose `contains` is just
//! a reference matches the id as a JSON string value, so other ids that contain it do not.

use std::collections::HashMap;

use serde_json::Value;

use super::condition::Check;
use crate::error::{Result, bail};

/// The references `text` makes, without their `$`.
pub fn tokens(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        let after = &rest[at + 1..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(after.len());
        if end > 0 {
            found.push(&after[..end]);
        }
        rest = &after[end..];
    }
    found
}

/// `text` with each reference replaced by its id.
fn replace(text: &str, ids: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(after.len());
        let name = &after[..end];
        if name.is_empty() {
            out.push('$');
        } else {
            match ids.get(name) {
                Some(id) => out.push_str(id),
                None => bail!("${name} names no entity: its spawn step did not report an id."),
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Every reference in the strings of `value`.
pub fn in_value(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) => found.extend(tokens(text).into_iter().map(str::to_owned)),
        Value::Array(items) => items.iter().for_each(|item| in_value(item, found)),
        Value::Object(fields) => fields.values().for_each(|item| in_value(item, found)),
        _ => {}
    }
}

/// Replace the references in the strings of `value`.
pub fn substitute(value: &mut Value, ids: &HashMap<String, String>) -> Result<()> {
    match value {
        Value::String(text) if text.contains('$') => *text = replace(text, ids)?,
        Value::Array(items) => {
            for item in items {
                substitute(item, ids)?;
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                substitute(item, ids)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Every reference in a condition.
pub fn in_check(check: &Check, found: &mut Vec<String>) {
    check.visit(&mut |text| found.extend(tokens(text).into_iter().map(str::to_owned)));
}

/// Replace the references in a condition; a `contains` that is one reference becomes the id
/// in quotes.
pub fn substitute_check(check: &mut Check, ids: &HashMap<String, String>) -> Result<()> {
    let mut failed = None;
    check.visit_mut(&mut |text| {
        if failed.is_some() || !text.contains('$') {
            return;
        }
        let whole = text.starts_with('$')
            && tokens(text)
                .first()
                .is_some_and(|name| name.len() + 1 == text.len());
        match replace(text, ids) {
            Ok(id) if whole => *text = format!("\"{id}\""),
            Ok(replaced) => *text = replaced,
            Err(error) => failed = Some(error),
        }
    });
    failed.map_or(Ok(()), Err)
}
