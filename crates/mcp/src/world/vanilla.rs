//! Reads that vanilla commands answer when the bedrock-mcp Bridge pack is absent (the world
//! needs cheats): tags, scoreboards, time, weather, game rules, entity properties and blocks.
//!
//! The game words its replies in the player's language. The parsers know the English texts of
//! Mojang's vanilla `resource_pack/texts/en_US.lang` (bedrock-samples), cited at each one, and
//! fail on any other form instead of guessing.

use serde_json::{Map, Value, json};

use super::{Target, World, dimension_id, number};
use crate::{
    bridge::Actor,
    error::{Error, Result, bail},
};

/// The selector radius around an entity's querytarget spot: under half the distance to the
/// nearest other entity, so the spot holds only it, but at most [`MAX_LOCATE_RADIUS`]. A wide
/// spot keeps a moving entity selected between re-reads.
const MAX_LOCATE_RADIUS: f64 = 2.0;
/// The smallest spot radius. querytarget prints positions to more decimals than [`number`]
/// keeps, so the spot must stay wider than that rounding.
const MIN_LOCATE_RADIUS: f64 = 0.05;
/// The share of the distance to the nearest other entity that the spot radius takes.
const LOCATE_SHARE: f64 = 0.45;
/// querytarget gives a player's eye, which stands this high above its feet; selectors
/// measure from the feet.
const EYE_HEIGHT: f64 = 1.62;
/// How many times in a row one read may find the entity gone from its spot and read the
/// spot again before the call fails.
const MAX_RELOCATIONS: usize = 5;
/// Integer and float properties are searched in the 32-bit integer range.
pub const PROPERTY_MIN: f64 = -2_147_483_648.0;
pub const PROPERTY_MAX: f64 = 2_147_483_647.0;
/// A float property between two integers is bisected to this precision.
pub const PROPERTY_PRECISION: f64 = 0.001;
/// `commands.generic.noTargetMatch`: a selector matched nothing.
const NO_TARGETS: &str = "No targets matched selector";

/// The failure for a reply in a form the parsers do not know.
fn unknown_form(command: &str, message: &str) -> Error {
    Error::msg(format!(
        "/{command} answered {message:?}, which is not the English (en_US) reply bedrock-mcp \
         reads. Set the game's language to English (United States), or add the bedrock-mcp \
         Bridge pack."
    ))
}

/// The reply text without `§` formatting codes.
fn plain(message: &str) -> String {
    let mut text = String::with_capacity(message.len());
    let mut chars = message.chars();
    while let Some(char) = chars.next() {
        if char == '§' {
            chars.next();
        } else {
            text.push(char);
        }
    }
    text
}

fn status(body: &Value) -> i64 {
    body["statusCode"].as_i64().unwrap_or(0)
}

fn message(body: &Value) -> &str {
    body["statusMessage"].as_str().unwrap_or_default()
}

/// The tags in a `tag <one target> list` reply: `commands.tag.list.single.success`
/// (`%1$s has %2$d tags: %3$s`) or `commands.tag.list.single.empty` (`%s has no tags`).
/// Names can hold " has ", so the first split whose list has the stated count wins.
pub fn parse_tag_list(message: &str) -> Option<Vec<String>> {
    let text = plain(message);
    let text = text.trim();
    if text.ends_with(" has no tags") {
        return Some(Vec::new());
    }
    text.match_indices(" has ").find_map(|(at, _)| {
        let rest = &text[at + " has ".len()..];
        let (count, list) = rest.split_once(" tags: ")?;
        let count: usize = count.parse().ok()?;
        let tags: Vec<String> = list.split(", ").map(str::to_owned).collect();
        (tags.len() == count).then_some(tags)
    })
}

/// One objective of `scoreboard objectives list`.
#[derive(Debug, PartialEq)]
pub struct Objective {
    pub id: String,
    pub display_name: String,
    pub criteria: String,
}

/// The lines of a reply, trimmed, without blank ones.
fn lines(message: &str) -> Vec<String> {
    plain(message)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `scoreboard objectives list`: `commands.scoreboard.objectives.list.empty`, or
/// `commands.scoreboard.objectives.list.count` (`Showing %1$d objective(s) on scoreboard:`)
/// and one `commands.scoreboard.objectives.list.entry`
/// (`- %1$s: displays as '%2$s' and is type '%3$s'`) per objective.
pub fn parse_objectives(message: &str) -> Option<Vec<Objective>> {
    let lines = lines(message);
    let (header, entries) = lines.split_first()?;
    if header == "There are no objectives on the scoreboard" && entries.is_empty() {
        return Some(Vec::new());
    }
    let count: usize = header
        .strip_prefix("Showing ")?
        .strip_suffix(" objective(s) on scoreboard:")?
        .parse()
        .ok()?;
    let objectives = entries
        .iter()
        .map(|line| {
            let entry = line.strip_prefix("- ")?.strip_suffix('\'')?;
            let (named, criteria) = entry.rsplit_once("' and is type '")?;
            let (id, display_name) = named.split_once(": displays as '")?;
            Some(Objective {
                id: id.to_owned(),
                display_name: display_name.to_owned(),
                criteria: criteria.to_owned(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (objectives.len() == count).then_some(objectives)
}

/// One score of `scoreboard players list *`.
#[derive(Debug, PartialEq)]
pub struct Score {
    pub participant: String,
    pub objective: String,
    pub score: i64,
}

/// `scoreboard players list *`: `commands.scoreboard.players.list.empty`, or per participant
/// `commands.scoreboard.players.list.player.count` (`Showing %1$d tracked objective(s) for
/// %2$s:`) and one `commands.scoreboard.players.list.player.entry` (`- %2$s: %1$d (%3$s)`:
/// display name, score, objective id) per score, or
/// `commands.scoreboard.players.list.player.empty` (`Player %1$s has no scores recorded`).
pub fn parse_scores(message: &str) -> Option<Vec<Score>> {
    let lines = lines(message);
    if lines == ["There are no tracked players on the scoreboard"] {
        return Some(Vec::new());
    }
    let mut scores = Vec::new();
    let mut current: Option<(String, usize)> = None;
    let close = |current: &mut Option<(String, usize)>| match current.take() {
        Some((_, left)) => left == 0,
        None => true,
    };
    for line in &lines {
        if let Some(header) = line.strip_prefix("Showing ")
            && let Some((count, participant)) = header.split_once(" tracked objective(s) for ")
            && let Some(participant) = participant.strip_suffix(':')
        {
            if !close(&mut current) {
                return None;
            }
            current = Some((participant.to_owned(), count.parse().ok()?));
        } else if line.starts_with("Player ") && line.ends_with(" has no scores recorded") {
            if !close(&mut current) {
                return None;
            }
        } else {
            let (participant, left) = current.as_mut()?;
            let entry = line.strip_prefix("- ")?.strip_suffix(')')?;
            let (shown, objective) = entry.rsplit_once(" (")?;
            let (_, score) = shown.rsplit_once(": ")?;
            *left = left.checked_sub(1)?;
            scores.push(Score {
                participant: participant.clone(),
                objective: objective.to_owned(),
                score: score.parse().ok()?,
            });
        }
    }
    close(&mut current).then_some(scores)
}

/// A `time query` value: the reply's `data`, else `commands.time.query.<query>`
/// (`Daytime is %d`, `Gametime is %d`, `Day is %d`).
pub fn parse_time(query: &str, body: &Value) -> Option<i64> {
    if let Some(value) = body["data"].as_i64() {
        return Some(value);
    }
    let prefix = match query {
        "daytime" => "Daytime is ",
        "gametime" => "Gametime is ",
        "day" => "Day is ",
        _ => return None,
    };
    plain(message(body))
        .trim()
        .strip_prefix(prefix)?
        .parse()
        .ok()
}

/// The block a failed `testforblock` found: `commands.testforblock.failed.tile`
/// (`The block at %d,%d,%d is %s (expected: %s).`), the block's display name.
pub fn parse_testforblock(message: &str) -> Option<String> {
    let text = plain(message);
    let rest = text.trim().strip_prefix("The block at ")?;
    let (_, found) = rest.split_once(" is ")?;
    let (block, _) = found.rsplit_once(" (expected: ")?;
    Some(block.to_owned())
}

/// `weather query`: `commands.weather.query` (`Weather state is: %s`) with
/// `commands.weather.query.clear|rain|thunder`, as the Script API's `WeatherType` names them.
pub fn parse_weather(message: &str) -> Option<&'static str> {
    match plain(message).trim().strip_prefix("Weather state is: ")? {
        "clear" => Some("Clear"),
        "rain" => Some("Rain"),
        "thunder" => Some("Thunder"),
        _ => None,
    }
}

/// `gamerule` without arguments. en_US.lang has no text for the list; the game prints
/// `<rule> = <value>` pairs joined by `, `, so a reply in any other shape is refused.
/// LeviLamina's `GameRuleCommand::createJsonIndex` shows the command also builds a JSON value
/// per rule, so fields beside the status in the reply are taken when the text does not parse.
pub fn parse_game_rules(body: &Value) -> Option<Map<String, Value>> {
    let text = plain(message(body));
    let parsed = text
        .trim()
        .split(", ")
        .map(|pair| {
            let (rule, value) = pair.split_once(" = ")?;
            if rule.is_empty() || rule.contains(char::is_whitespace) {
                return None;
            }
            let value = match value {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value
                    .parse::<i64>()
                    .map(Value::from)
                    .or_else(|_| value.parse::<f64>().map(Value::from))
                    .unwrap_or_else(|_| value.into()),
            };
            Some((rule.to_owned(), value))
        })
        .collect::<Option<Map<_, _>>>();
    if let Some(rules) = parsed.filter(|rules| !rules.is_empty()) {
        return Some(rules);
    }
    let fields: Map<String, Value> = body
        .as_object()?
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "statusCode" | "statusMessage"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!fields.is_empty()).then_some(fields)
}

/// Find a number in [`PROPERTY_MIN`], [`PROPERTY_MAX`] with `within(value)`, which says
/// whether the property matches `value` (`n` or `low..high`, inclusive). Integer ranges
/// come first, so an int property only meets integer bounds; a value between two integers
/// is then bisected to [`PROPERTY_PRECISION`]. `None` when the full range does not match;
/// the flag says whether the value matched exactly.
pub fn bisect_property(
    mut within: impl FnMut(&str) -> Result<bool>,
) -> Result<Option<(f64, bool)>> {
    let range = |low: f64, high: f64| format!("{}..{}", number(low), number(high));
    let (mut low, mut high) = (PROPERTY_MIN, PROPERTY_MAX);
    if !within(&range(low, high))? {
        return Ok(None);
    }
    while high - low > 1.0 {
        let middle = ((low + high) / 2.0).floor();
        if within(&range(low, middle))? {
            high = middle;
        } else {
            low = middle;
        }
    }
    for whole in [low, high] {
        if within(&number(whole))? {
            return Ok(Some((whole, true)));
        }
    }
    while high - low > PROPERTY_PRECISION {
        let middle = (low + high) / 2.0;
        if within(&range(low, middle))? {
            high = middle;
        } else {
            low = middle;
        }
    }
    Ok(Some((
        ((low + high) / 2.0 * 1000.0).round() / 1000.0,
        false,
    )))
}

/// An entity selected for vanilla commands, which cannot name an entity by id: a player by
/// name, anything else by a spot around where querytarget saw it (and by its type when
/// known), checked to hold only that entity. See [`World::on_located`].
#[derive(Clone, Debug)]
pub struct Located {
    pub id: String,
    pub selector: String,
    pub target: Target,
    /// The player's name, for players.
    pub player: Option<String>,
    /// The entity's type, such as `minecraft:bee`, when a native read knew it.
    pub kind: Option<String>,
}

/// `selector` with one more argument.
fn with_argument(selector: &str, argument: &str) -> String {
    let open = selector.strip_suffix(']').unwrap_or(selector);
    format!("{open},{argument}]")
}

/// The spot radius for `target` among `all` querytarget results; see [`MAX_LOCATE_RADIUS`].
/// Every other position also counts as a player's eye, whose feet are lower, so a player
/// beside the entity narrows the spot as much as its feet would.
fn locate_radius(target: &Target, all: &[Target]) -> f64 {
    let nearest = all
        .iter()
        .filter(|other| other.unique_id != target.unique_id && other.dimension == target.dimension)
        .flat_map(|other| {
            let [x, y, z] = other.position;
            [other.position, [x, y - EYE_HEIGHT, z]]
        })
        .map(|position| super::distance(position, target.position))
        .fold(f64::INFINITY, f64::min);
    (nearest * LOCATE_SHARE).clamp(MIN_LOCATE_RADIUS, MAX_LOCATE_RADIUS)
}

/// What a property probe answered.
enum Probe {
    Match,
    NoMatch,
    /// The game refused the selector; its message.
    Refused(String),
}

/// One entity property read by `has_property` probes.
#[derive(Debug, PartialEq)]
pub enum PropertyRead {
    /// The value; `exact` is false for a float found to [`PROPERTY_PRECISION`].
    Value { value: Value, exact: bool },
    /// Why no probe matched.
    Unread(String),
}

impl World {
    /// A command's tags: `tag <selector> list`, which needs `selector` to match one entity.
    pub fn tags(&self, selector: &str) -> Result<Vec<String>> {
        let command = format!("tag {selector} list");
        let body = self.raw(&command)?;
        if let Some(tags) = parse_tag_list(message(&body)) {
            return Ok(tags);
        }
        if status(&body) != 0 {
            bail!("/{command}: {}", message(&body));
        }
        Err(unknown_form(&command, message(&body)))
    }

    /// Find entity `id` so commands can select it; see [`Located`]. `kind`, the entity's type
    /// when known, narrows the selector.
    pub fn locate(&self, id: &str, kind: Option<&str>) -> Result<Located> {
        let players = self.targets("@a")?;
        if let Some(index) = players.iter().position(|target| target.unique_id == id) {
            let names = self.raw("testfor @a")?["victim"].clone();
            let Some(name) = names.get(index).and_then(Value::as_str) else {
                bail!("testfor @a did not name player {id}.");
            };
            let selector = Actor::try_from(name.to_owned())
                .map_err(Error::msg)?
                .selector();
            return Ok(Located {
                id: id.to_owned(),
                selector,
                target: players[index].clone(),
                player: Some(name.to_owned()),
                kind: kind.map(str::to_owned),
            });
        }
        let all = self.targets("@e")?;
        let Some(target) = all.iter().find(|target| target.unique_id == id) else {
            bail!(
                "No entity with id {id} matched querytarget @e: it died, despawned, or the id \
                 is wrong. entities_list shows the current ids."
            );
        };
        let me = self.actor_target(&Actor::Me)?;
        if me.dimension != target.dimension {
            bail!(
                "Entity {id} is in {}, not in the player's dimension, and commands select it by \
                 position only there.",
                dimension_id(&target.dimension)
            );
        }
        let radius = locate_radius(target, &all);
        let [x, y, z] = target.position.map(number);
        let filter = kind.map(|kind| format!("type={kind},")).unwrap_or_default();
        let located = Located {
            id: id.to_owned(),
            selector: format!("@e[{filter}x={x},y={y},z={z},r={}]", number(radius)),
            target: target.clone(),
            player: None,
            kind: kind.map(str::to_owned),
        };
        match self.at_spot(&located)?.as_slice() {
            [only] if only.unique_id == id => Ok(located),
            [] => bail!(
                "Entity {id} left its spot as soon as querytarget saw it, so commands could not \
                 select it; read it again."
            ),
            found => bail!(
                "{} entities stand within {} blocks of entity {id}, so commands cannot select \
                 it alone; move them apart or add the bedrock-mcp Bridge pack.",
                found.len(),
                number(radius)
            ),
        }
    }

    /// What the located entity's selector picks now.
    fn at_spot(&self, located: &Located) -> Result<Vec<Target>> {
        self.targets(&located.selector)
    }

    /// Run `read` with the located entity's selector. When the spot no longer holds only that
    /// entity afterwards, the answer may be about another entity or none, so the entity is
    /// located again from a fresh querytarget and `read` runs again, at most
    /// [`MAX_RELOCATIONS`] times in a row. Players are selected by name and need no check.
    pub fn on_located<T>(
        &self,
        located: &mut Located,
        mut read: impl FnMut(&str) -> Result<T>,
    ) -> Result<T> {
        if located.player.is_some() {
            return read(&located.selector);
        }
        for _ in 0..=MAX_RELOCATIONS {
            let answer = read(&located.selector)?;
            if let [only] = self.at_spot(located)?.as_slice()
                && only.unique_id == located.id
            {
                located.target = only.clone();
                return Ok(answer);
            }
            *located = self.locate(&located.id, located.kind.as_deref())?;
        }
        bail!(
            "Entity {} kept leaving the spot commands select it by: {} reads in a row found it \
             gone afterwards. Read it again when it moves less, or add the bedrock-mcp Bridge \
             pack.",
            located.id,
            MAX_RELOCATIONS + 1
        )
    }

    /// The display name `testfor` gives for `selector`'s first entity.
    pub fn display_name(&self, selector: &str) -> Result<Option<String>> {
        Ok(self.raw(&format!("testfor {selector}"))?["victim"][0]
            .as_str()
            .map(str::to_owned))
    }

    /// Whether the located entity matches `has_property={name=value}`.
    fn probe(&self, located: &mut Located, name: &str, value: &str) -> Result<Probe> {
        let filter = format!("has_property={{{name}={value}}}");
        self.on_located(located, |selector| {
            let body = self.raw(&format!("testfor {}", with_argument(selector, &filter)))?;
            Ok(if status(&body) == 0 {
                Probe::Match
            } else if message(&body).contains(NO_TARGETS) {
                Probe::NoMatch
            } else {
                Probe::Refused(message(&body).to_owned())
            })
        })
    }

    /// Like [`World::probe`], but a refusal is an error: used once the type is known.
    fn matches(&self, located: &mut Located, name: &str, value: &str) -> Result<bool> {
        match self.probe(located, name, value)? {
            Probe::Match => Ok(true),
            Probe::NoMatch => Ok(false),
            Probe::Refused(message) => bail!("testfor with has_property {name}={value}: {message}"),
        }
    }

    /// Read entity property `name` with `has_property` probes: each of `enum_values` when
    /// given, else `true` and `false`, then a bisection over numbers.
    pub fn property(
        &self,
        located: &mut Located,
        name: &str,
        enum_values: Option<&[String]>,
    ) -> Result<PropertyRead> {
        let mut refused = None;
        let mut note = |probe: Probe| match probe {
            Probe::Match => true,
            Probe::NoMatch => false,
            Probe::Refused(message) => {
                refused = Some(message);
                false
            }
        };
        if let Some(values) = enum_values {
            for value in values {
                if note(self.probe(located, name, &format!("\"{value}\""))?) {
                    return Ok(PropertyRead::Value {
                        value: value.as_str().into(),
                        exact: true,
                    });
                }
            }
            let reason = format!(
                "It is none of the enum_values given ({}).",
                values.join(", ")
            );
            return Ok(PropertyRead::Unread(with_refusal(reason, refused)));
        }
        for flag in [true, false] {
            if note(self.probe(located, name, &flag.to_string())?) {
                return Ok(PropertyRead::Value {
                    value: flag.into(),
                    exact: true,
                });
            }
        }
        let full = format!("{}..{}", number(PROPERTY_MIN), number(PROPERTY_MAX));
        if note(self.probe(located, name, &full)?) {
            let found = bisect_property(|value| self.matches(located, name, value))?;
            if let Some((value, exact)) = found {
                let value = if exact {
                    json!(value as i64)
                } else {
                    json!(value)
                };
                return Ok(PropertyRead::Value { value, exact });
            }
        }
        let reason = format!(
            "It is not a boolean or a number from {} to {}: an enum property (pass its values in \
             enum_values), or one the entity's type does not define.",
            number(PROPERTY_MIN),
            number(PROPERTY_MAX)
        );
        Ok(PropertyRead::Unread(with_refusal(reason, refused)))
    }

    /// Time, weather and game rules from `time query`, `weather query` and `gamerule`, and the
    /// dimension of the player who ran /connect.
    pub fn command_world_info(&self) -> Result<Value> {
        let time = |query: &str| -> Result<i64> {
            let command = format!("time query {query}");
            let body = self.command(&command)?;
            parse_time(query, &body).ok_or_else(|| unknown_form(&command, message(&body)))
        };
        let weather = self.command("weather query")?;
        let weather = parse_weather(message(&weather))
            .ok_or_else(|| unknown_form("weather query", message(&weather)))?;
        let rules = self.command("gamerule")?;
        let rules =
            parse_game_rules(&rules).ok_or_else(|| unknown_form("gamerule", message(&rules)))?;
        let me = self.actor_target(&Actor::Me)?;
        Ok(json!({
            "timeOfDay": time("daytime")?,
            "absoluteTime": time("gametime")?,
            "day": time("day")?,
            "weather": weather,
            "dimension": dimension_id(&me.dimension),
            "gameRules": rules,
        }))
    }

    /// Objectives and their scores from `scoreboard objectives list` and
    /// `scoreboard players list *`: every objective, or the one named.
    pub fn command_scores(&self, objective: Option<&str>) -> Result<Vec<Value>> {
        let command = "scoreboard objectives list";
        let body = self.raw(command)?;
        let objectives = match parse_objectives(message(&body)) {
            Some(objectives) => objectives,
            None if status(&body) != 0 => bail!("/{command}: {}", message(&body)),
            None => return Err(unknown_form(command, message(&body))),
        };
        let objectives: Vec<Objective> = objectives
            .into_iter()
            .filter(|found| objective.is_none_or(|wanted| found.id == wanted))
            .collect();
        if objectives.is_empty() {
            return Ok(Vec::new());
        }
        let command = "scoreboard players list *";
        let body = self.raw(command)?;
        let scores = match parse_scores(message(&body)) {
            Some(scores) => scores,
            None if status(&body) != 0 => bail!("/{command}: {}", message(&body)),
            None => return Err(unknown_form(command, message(&body))),
        };
        Ok(objectives
            .iter()
            .map(|found| {
                let scores: Vec<Value> = scores
                    .iter()
                    .filter(|score| score.objective == found.id)
                    .map(|score| {
                        json!({ "participant": score.participant, "type": null, "score": score.score })
                    })
                    .collect();
                json!({
                    "id": found.id,
                    "displayName": found.display_name,
                    "criteria": found.criteria,
                    "scores": scores,
                })
            })
            .collect())
    }
}

/// A reason, with the game's refusal of a probe when there was one.
fn with_refusal(reason: String, refused: Option<String>) -> String {
    match refused {
        Some(message) => format!("{reason} The game refused a probe: {message}"),
        None => reason,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::bridge::fake::{failure, game};

    #[test]
    fn tag_lists_parse_in_english_only() {
        assert_eq!(parse_tag_list("Bob has no tags"), Some(vec![]));
        assert_eq!(
            parse_tag_list("Bob has 2 tags: §aready§r, §ablue team§r"),
            Some(vec!["ready".to_owned(), "blue team".to_owned()])
        );
        // A name holding " has " still splits where the count fits.
        assert_eq!(
            parse_tag_list("Zed has 3 friends has 1 tags: x"),
            Some(vec!["x".to_owned()])
        );
        assert_eq!(parse_tag_list("Bob a 2 tags : a, b"), None);
    }

    #[test]
    fn scoreboard_replies_parse() {
        let objectives = parse_objectives(
            "Showing 2 objective(s) on scoreboard:\n- kills: displays as 'Kills: total' and is type 'dummy'\n- coins: displays as 'Coins' and is type 'dummy'",
        )
        .unwrap();
        assert_eq!(objectives[0].id, "kills");
        assert_eq!(objectives[0].display_name, "Kills: total");
        assert_eq!(
            parse_objectives("There are no objectives on the scoreboard"),
            Some(vec![])
        );
        assert_eq!(
            parse_objectives(
                "Showing 2 objective(s) on scoreboard:\n- kills: displays as 'K' and is type 'dummy'"
            ),
            None
        );
        let scores = parse_scores(
            "Showing 2 tracked objective(s) for Bob:\n- Kills: total: 3 (kills)\n- Coins: -7 (coins)\nPlayer Ann has no scores recorded\nShowing 1 tracked objective(s) for fake:\n- Coins: 12 (coins)",
        )
        .unwrap();
        assert_eq!(
            scores,
            [
                Score {
                    participant: "Bob".into(),
                    objective: "kills".into(),
                    score: 3
                },
                Score {
                    participant: "Bob".into(),
                    objective: "coins".into(),
                    score: -7
                },
                Score {
                    participant: "fake".into(),
                    objective: "coins".into(),
                    score: 12
                },
            ]
        );
        assert_eq!(
            parse_scores("There are no tracked players on the scoreboard"),
            Some(vec![])
        );
        assert_eq!(parse_scores("Affichage de 2 objectifs"), None);
        assert_eq!(
            parse_scores("Showing 2 tracked objective(s) for Bob:\n- C: 1 (c)"),
            None
        );
    }

    #[test]
    fn world_replies_parse() {
        assert_eq!(
            parse_time("day", &json!({"statusMessage": "Day is 4"})),
            Some(4)
        );
        assert_eq!(
            parse_time("daytime", &json!({"statusMessage": "x", "data": 6000})),
            Some(6000)
        );
        assert_eq!(parse_weather("Weather state is: thunder"), Some("Thunder"));
        assert_eq!(parse_weather("Wetter: Regen"), None);
        let rules = parse_game_rules(&json!({
            "statusMessage": "commandblockoutput = true, randomtickspeed = 1, spawnradius = 5"
        }))
        .unwrap();
        assert_eq!(rules["commandblockoutput"], true);
        assert_eq!(rules["randomtickspeed"], 1);
        let from_fields = parse_game_rules(&json!({
            "statusCode": 0, "statusMessage": "?", "dodaylightcycle": false
        }))
        .unwrap();
        assert_eq!(from_fields["dodaylightcycle"], false);
        assert!(parse_game_rules(&json!({"statusCode": 0, "statusMessage": "?"})).is_none());
    }

    /// Whether `value` (`n` or `low..high`) holds `actual`.
    fn holds(value: &str, actual: f64) -> bool {
        match value.split_once("..") {
            Some((low, high)) => {
                low.parse::<f64>().unwrap() <= actual && actual <= high.parse::<f64>().unwrap()
            }
            None => value.parse::<f64>().unwrap() == actual,
        }
    }

    #[test]
    fn property_bisection_finds_ints_exactly_and_floats_closely() {
        for actual in [0.0, -1.0, 7.0, PROPERTY_MAX, PROPERTY_MIN, 123_456.0] {
            let mut tests = 0;
            let found = bisect_property(|value| {
                tests += 1;
                assert!(!value.contains('.') || value.contains(".."), "{value}");
                Ok(holds(value, actual))
            })
            .unwrap();
            assert_eq!(found, Some((actual, true)), "{actual}");
            assert!(tests <= 35, "{actual}: {tests}");
        }
        for actual in [2.5, -0.3, 1000.125] {
            let (found, exact) = bisect_property(|value| Ok(holds(value, actual)))
                .unwrap()
                .unwrap();
            assert!(
                !exact && (found - actual).abs() <= PROPERTY_PRECISION,
                "{actual} {found}"
            );
        }
        assert_eq!(bisect_property(|_| Ok(false)).unwrap(), None);
    }

    const ME: &str =
        r#"{"dimension":0,"id":-1,"uniqueId":"-1","position":{"x":0,"y":1.62,"z":0},"yRot":0}"#;
    const COW: &str =
        r#"{"dimension":0,"id":-9,"uniqueId":"-9","position":{"x":3.5,"y":64,"z":-2.25},"yRot":0}"#;
    const COW_SPOT: &str = "@e[x=3.5,y=64,z=-2.25,r=2";

    /// A cow at (3.5, 64, -2.25) with tag "pet", wiki:tame true and wiki:age 7.
    fn cow_world(line: &str) -> Value {
        let details =
            |list: &[&str]| json!({"statusCode": 0, "details": format!("[{}]", list.join(","))});
        let no_match = || json!({"statusCode": 1, "statusMessage": "No targets matched selector"});
        match line {
            "querytarget @a" | "querytarget @s" => return details(&[ME]),
            "querytarget @e" => return details(&[ME, COW]),
            "testfor @a" => return json!({"statusCode": 0, "victim": ["Bob"]}),
            _ => {}
        }
        if line == format!("querytarget {COW_SPOT}]") {
            return details(&[COW]);
        }
        if line == format!("tag {COW_SPOT}] list") {
            return json!({"statusCode": 0, "statusMessage": "Cow has 1 tags: §apet§r"});
        }
        if line == "tag @a[name=\"Bob\"] list" {
            return json!({"statusCode": 0, "statusMessage": "Bob has no tags"});
        }
        if let Some(filter) = line
            .strip_prefix(&format!("testfor {COW_SPOT},has_property={{"))
            .and_then(|rest| rest.strip_suffix("}]"))
        {
            let (name, value) = filter.split_once('=').unwrap();
            let matched = match name {
                "wiki:tame" => value == "true",
                "wiki:age" => value != "true" && value != "false" && holds(value, 7.0),
                "wiki:color" => value == "\"red\"",
                "wiki:bad" => return failure("Syntax error: Unexpected \"wiki:bad\""),
                _ => false,
            };
            return if matched {
                json!({"statusCode": 0, "victim": ["Cow"]})
            } else {
                no_match()
            };
        }
        failure(&format!("unexpected {line}"))
    }

    #[test]
    fn entities_are_located_by_spot_and_read_with_probes() {
        let (world, seen) = game(cow_world);
        let mut cow = world.locate("-9", None).unwrap();
        assert_eq!(cow.selector, format!("{COW_SPOT}]"));
        assert_eq!(world.tags(&cow.selector).unwrap(), ["pet"]);
        let mut read =
            |name: &str, values: Option<&[String]>| world.property(&mut cow, name, values).unwrap();
        assert_eq!(
            read("wiki:tame", None),
            PropertyRead::Value {
                value: json!(true),
                exact: true
            }
        );
        assert_eq!(
            read("wiki:age", None),
            PropertyRead::Value {
                value: json!(7),
                exact: true
            }
        );
        let colors = ["blue".to_owned(), "red".to_owned()];
        assert_eq!(
            read("wiki:color", Some(&colors)),
            PropertyRead::Value {
                value: json!("red"),
                exact: true
            }
        );
        assert!(
            matches!(read("wiki:color", None), PropertyRead::Unread(reason) if reason.contains("enum_values"))
        );
        assert!(
            matches!(read("wiki:bad", None), PropertyRead::Unread(reason) if reason.contains("Syntax error"))
        );
        let bob = world.locate("-1", None).unwrap();
        assert_eq!(bob.selector, "@a[name=\"Bob\"]");
        assert!(world.tags(&bob.selector).unwrap().is_empty());
        assert!(
            world
                .locate("-77", None)
                .unwrap_err()
                .to_string()
                .contains("No entity with id -77")
        );
        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|line| !line.contains("c=1")), "{seen:?}");
    }

    #[test]
    fn crowded_spots_and_other_languages_fail_with_the_reason() {
        let crowd = Arc::new(Mutex::new(false));
        let crowded = Arc::clone(&crowd);
        let (world, _) = game(move |line| {
            if *crowded.lock().unwrap() && line == format!("querytarget {COW_SPOT}]") {
                return json!({"statusCode": 0, "details": format!("[{COW},{COW}]")});
            }
            if line.starts_with("tag ") {
                return json!({"statusCode": 0, "statusMessage": "Cow a 1 étiquettes : pet"});
            }
            cow_world(line)
        });
        let mut cow = world.locate("-9", None).unwrap();
        let error = world.tags(&cow.selector).unwrap_err().to_string();
        assert!(error.contains("English"), "{error}");
        *crowd.lock().unwrap() = true;
        let error = world
            .on_located(&mut cow, |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("2 entities stand within"), "{error}");
    }

    #[test]
    fn the_spot_is_wide_but_under_half_the_way_to_the_nearest_other_entity() {
        let at = |id: &str, [x, y, z]: [f64; 3]| Target {
            id: Value::Null,
            unique_id: id.to_owned(),
            dimension: json!(0),
            position: [x, y, z],
            yaw: 0.0,
        };
        let bee = at("-2", [0.0, 64.0, 0.0]);
        let alone = [bee.clone(), at("-3", [50.0, 64.0, 0.0])];
        assert_eq!(locate_radius(&bee, &alone), MAX_LOCATE_RADIUS);
        let pair = [bee.clone(), at("-3", [1.0, 64.0, 0.0])];
        assert!((locate_radius(&bee, &pair) - 0.45).abs() < 1e-9);
        // A player's eye 2.62 above the bee puts its feet 1 block above it.
        let player = [bee.clone(), at("-1", [0.0, 66.62, 0.0])];
        assert!((locate_radius(&bee, &player) - 0.45).abs() < 1e-9);
        let crowded = [bee.clone(), at("-3", [0.01, 64.0, 0.0])];
        assert_eq!(locate_radius(&bee, &crowded), MIN_LOCATE_RADIUS);
    }

    /// A bee at (x, 64, 0) without nectar that flies 3 blocks along x during each of the
    /// next `flights` probes, so the probe looks at the spot it left.
    fn bee_world(flights: usize) -> (World, crate::bridge::fake::Seen) {
        let state = Arc::new(Mutex::new((10.0_f64, flights)));
        game(move |line| {
            let mut state = state.lock().unwrap();
            let bee = |x: f64| {
                format!(
                    r#"{{"dimension":0,"id":-2,"uniqueId":"-2","position":{{"x":{x},"y":64,"z":0}},"yRot":0}}"#
                )
            };
            let details = |list: &[String]| json!({"statusCode": 0, "details": format!("[{}]", list.join(","))});
            let spot = format!("@e[type=minecraft:bee,x={},y=64,z=0,r=2", number(state.0));
            let no_match =
                || json!({"statusCode": 1, "statusMessage": "No targets matched selector"});
            match line {
                "querytarget @a" | "querytarget @s" => details(&[ME.to_owned()]),
                "querytarget @e" => details(&[ME.to_owned(), bee(state.0)]),
                _ if line.starts_with("querytarget @e[") => {
                    match line == format!("querytarget {spot}]") {
                        true => details(&[bee(state.0)]),
                        false => details(&[]),
                    }
                }
                _ if line.starts_with("testfor @e[") => {
                    if state.1 > 0 {
                        state.1 -= 1;
                        state.0 += 3.0;
                    }
                    let here = line.starts_with(&format!("testfor {spot},"));
                    match here && line.ends_with("has_property={minecraft:has_nectar=false}]") {
                        true => json!({"statusCode": 0, "victim": ["Bee"]}),
                        false => no_match(),
                    }
                }
                _ => failure(&format!("unexpected {line}")),
            }
        })
    }

    #[test]
    fn a_moving_entity_is_located_again_and_its_probe_repeated() {
        let (world, seen) = bee_world(2);
        let mut bee = world.locate("-2", Some("minecraft:bee")).unwrap();
        assert_eq!(bee.selector, "@e[type=minecraft:bee,x=10,y=64,z=0,r=2]");
        assert_eq!(
            world
                .property(&mut bee, "minecraft:has_nectar", None)
                .unwrap(),
            PropertyRead::Value {
                value: json!(false),
                exact: true
            }
        );
        assert_eq!(bee.selector, "@e[type=minecraft:bee,x=16,y=64,z=0,r=2]");
        let seen = seen.lock().unwrap();
        let probes = seen
            .iter()
            .filter(|line| line.contains("has_property"))
            .count();
        // true at x=10 (it flew), true at x=13 (it flew), true at x=16, then false.
        assert_eq!(probes, 4, "{seen:?}");
    }

    #[test]
    fn an_entity_that_keeps_moving_fails_after_bounded_relocations() {
        let (world, _) = bee_world(usize::MAX);
        let mut bee = world.locate("-2", Some("minecraft:bee")).unwrap();
        let error = world
            .property(&mut bee, "minecraft:has_nectar", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("kept leaving the spot"), "{error}");
    }

    #[test]
    fn world_info_and_scores_come_from_commands() {
        let (world, _) = game(|line| match line {
            "time query daytime" => {
                json!({"statusCode": 0, "statusMessage": "Daytime is 6000", "data": 6000})
            }
            "time query gametime" => json!({"statusCode": 0, "statusMessage": "Gametime is 30000"}),
            "time query day" => json!({"statusCode": 0, "statusMessage": "Day is 1"}),
            "weather query" => json!({"statusCode": 0, "statusMessage": "Weather state is: rain"}),
            "gamerule" => {
                json!({"statusCode": 0, "statusMessage": "dodaylightcycle = false, spawnradius = 5"})
            }
            "querytarget @s" => json!({"statusCode": 0, "details": format!("[{ME}]")}),
            "scoreboard objectives list" => {
                json!({"statusCode": 0, "statusMessage": "Showing 2 objective(s) on scoreboard:\n- kills: displays as 'Kills' and is type 'dummy'\n- coins: displays as 'Coins' and is type 'dummy'"})
            }
            "scoreboard players list *" => {
                json!({"statusCode": 0, "statusMessage": "Showing 1 tracked objective(s) for Bob:\n- Kills: 3 (kills)"})
            }
            _ => failure(&format!("unexpected {line}")),
        });
        let info = world.command_world_info().unwrap();
        assert_eq!(info["timeOfDay"], 6000);
        assert_eq!(info["absoluteTime"], 30000);
        assert_eq!(info["day"], 1);
        assert_eq!(info["weather"], "Rain");
        assert_eq!(info["dimension"], "minecraft:overworld");
        assert_eq!(info["gameRules"]["dodaylightcycle"], false);
        let all = world.command_scores(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0]["scores"][0],
            json!({"participant": "Bob", "type": null, "score": 3})
        );
        assert_eq!(all[1]["scores"], json!([]));
        let coins = world.command_scores(Some("coins")).unwrap();
        assert_eq!(coins.len(), 1);
        assert!(world.command_scores(Some("none")).unwrap().is_empty());
    }
}
