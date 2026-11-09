//! MCP over stdio: JSON-RPC 2.0, one message per line.

use std::{
    io::{self, BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use base64::Engine;
use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    prompts,
};

pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_NAME: &str = "bedrock-mcp";
const SERVER_TITLE: &str = "Bedrock MCP for Minecraft";
/// How to run a play test, for the client's model.
const INSTRUCTIONS: &str = "\
Testing an add-on in the game (prompts/list offers these flows ready to follow: playtest_item, \
record_combat, debug_pack_errors, test_ui_flow):
1. game_launch, then world_open {name} (worlds_list shows names and packs), then \
automation_connect (bridge: true means the world runs the Bridge pack), then scene_reset to set \
the place, entities, time, inventory and perspective in one call.
The Bridge pack is the user's choice. Never install it or add it to a world on your own: no \
packs_install without check: true, no world_packs_set for it, unless the user asked. Without it, \
single-player tests still run on native reads and vanilla commands (cheats on). Suggest it to \
the user, saying what it adds, when a reply says a tool needs it (inventory, items, blocks, \
raycasts, containers, approach) or when the test is in multiplayer, where native reads do not \
work. When the user agrees: packs_install, then world_packs_set while out of the world, then \
reopen it.
2. Held items need a warm-up after a world loads, or their attachable can stay frozen \
(player_render_get shows it): scene_reset ends with one (warm_up, default true), and \
action_sequence runs one before a recording or its first run in a newly loaded world (warm_up \
\"auto\"). Both report it.
3. Act with the actions tools and check each reply's confirmed and notes. For timed play \
(combos, attacks, throws), put the steps in one action_sequence instead of separate calls. \
movement_set {to} walks a path around walls; fight {target, all} melees mobs until they die. For \
long sequences pass background: true, watch with sequence_status, abort with sequence_stop. Use \
ui_click for menu and form buttons. When actions stop confirming, read game_status.attention \
(death or disconnect screens). The player's own input is locked out while the server plays; \
taken_over: true, or an error saying the player pressed F9, means the user took over, so ask \
before you play on.
4. camera_set picks first or third person. Read errors with content_log_read {summary: true}. \
To record a timed fight, pass record: true to its action_sequence; otherwise use video_start \
{pause_on_idle_ms: 500} / video_stop, which pauses the video while no tool call runs, so your \
thinking between calls stays out of it. Set everything up first and look at results afterwards.
5. After you edit the add-on's files, game_reload {packs: [...]} (or {project} for an rgl \
project) copies them over the folders the game loads, runs /reload all, waits for the world to \
reopen and returns the new errors with file and line. Never leave and reopen the world for an \
edit. pack_check {path} finds broken references without the game. A pack folder the game has \
never seen still needs a game restart.
GameTests without a client: gametest_server; a load check without a client: bds_check.";

/// One item of a tool result.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

impl Content {
    /// Pretty JSON text.
    pub fn json(value: &Value) -> Self {
        Self::Text(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!({ "type": "text", "text": text }),
            Self::Png(data) => json!({
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(data),
                "mimeType": "image/png",
            }),
        }
    }
}

pub trait ToolServer {
    /// The `tools/list` entries this server offers.
    fn tools(&self) -> Vec<Value>;
    /// Whether `tools/list` offers `name`.
    fn offers(&self, name: &str) -> bool;
    /// Run a listed tool. [`Error::Waiting`] asks to be called again later with the same
    /// arguments (`wait_ms` lowered to what is left of it), until it answers.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Vec<Content>>;
    /// The client went away: release what the session holds.
    fn close(&mut self);
    /// Run the work the last call queued (a background action sequence), calling `pump` at
    /// its wait points so the messages that arrive meanwhile get answered.
    fn work(&mut self, _pump: &mut dyn Pump) {}
    /// `gone` turns true once the client went away, so running work can stop early.
    fn attach_client(&mut self, _gone: Arc<AtomicBool>) {}
}

/// Answers messages while a server works.
pub trait Pump {
    /// Answer the messages that arrive within `wait`, and the calls that waited for news.
    /// It may return late when answering a message takes longer.
    fn pump(&mut self, server: &mut dyn ToolServer, wait: Duration);
}

fn reply(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The answer to a tool call: its content, or its failure as a tool error.
fn tool_reply(id: &Value, result: Result<Vec<Content>>) -> Value {
    match result {
        Ok(content) => reply(
            id,
            json!({ "content": content.iter().map(Content::to_json).collect::<Vec<_>>() }),
        ),
        Err(failure) => reply(
            id,
            json!({
                "content": [Content::json(&json!({ "error": failure.to_string() })).to_json()],
                "isError": true,
            }),
        ),
    }
}

/// A tool call that waits for background work.
struct Waiting {
    id: Value,
    name: String,
    arguments: Value,
    /// When its `wait_ms` runs out.
    until: Option<Instant>,
}

enum Response {
    Answer(Value),
    /// A notification.
    Quiet,
    Wait(Waiting),
}

/// Answer one message; `None` for notifications. A call that would wait for background
/// work gets a tool error, since only [`serve`] runs it.
#[cfg(test)]
pub fn handle(server: &mut dyn ToolServer, message: &Value) -> Option<Value> {
    match respond(server, message) {
        Response::Answer(answer) => Some(answer),
        Response::Quiet => None,
        Response::Wait(waiting) => Some(tool_reply(
            &waiting.id,
            Err(Error::msg(
                "This call waits for a background run, and none runs here.",
            )),
        )),
    }
}

fn respond(server: &mut dyn ToolServer, message: &Value) -> Response {
    let Some(id) = message.get("id") else {
        return Response::Quiet;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Response::Answer(error(id, -32600, "Invalid request: no method"));
    };
    Response::Answer(match method {
        "initialize" => {
            let requested = params["protocolVersion"].as_str();
            let version = PROTOCOL_VERSIONS
                .into_iter()
                .find(|v| Some(*v) == requested)
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            reply(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {}, "prompts": {} },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "title": SERVER_TITLE,
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => reply(id, json!({})),
        "tools/list" => reply(id, json!({ "tools": server.tools() })),
        "prompts/list" => reply(id, json!({ "prompts": prompts::list() })),
        "prompts/get" => {
            let Some(name) = params["name"].as_str() else {
                return Response::Answer(error(id, -32602, "prompts/get needs a prompt name"));
            };
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => serde_json::Map::new(),
                Some(Value::Object(arguments)) => arguments.clone(),
                Some(_) => {
                    return Response::Answer(error(
                        id,
                        -32602,
                        "Prompt arguments must be an object",
                    ));
                }
            };
            match prompts::get(name, &arguments) {
                Ok(result) => reply(id, result),
                Err(message) => error(id, -32602, &message),
            }
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Response::Answer(error(id, -32602, "tools/call needs a tool name"));
            };
            if !server.offers(name) {
                return Response::Answer(error(id, -32602, &format!("Unknown tool: {name}")));
            }
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(arguments @ Value::Object(_)) => arguments.clone(),
                Some(_) => {
                    return Response::Answer(error(id, -32602, "Tool arguments must be an object"));
                }
            };
            match server.call(name, arguments.clone()) {
                Err(Error::Waiting) => {
                    let until = arguments["wait_ms"]
                        .as_u64()
                        .map(|ms| Instant::now() + Duration::from_millis(ms));
                    return Response::Wait(Waiting {
                        id: id.clone(),
                        name: name.to_owned(),
                        arguments,
                        until,
                    });
                }
                result => tool_reply(id, result),
            }
        }
        _ => error(id, -32601, &format!("Method not found: {method}")),
    })
}

/// What the reader thread passes on.
enum Incoming {
    Line(String),
    /// The input ended, or failed to read.
    End(Option<io::Error>),
}

/// The client's side of [`serve`]: lines from the reader thread, the output, and the calls
/// that wait for background work.
struct Link<W: Write> {
    lines: Receiver<Incoming>,
    output: W,
    waiting: Vec<Waiting>,
    /// The input ended or the output failed: the client is gone.
    ended: bool,
    failure: Option<io::Error>,
    gone: Arc<AtomicBool>,
}

impl<W: Write> Link<W> {
    fn send(&mut self, answer: &Value) {
        if self.failure.is_some() {
            return;
        }
        if let Err(failure) = writeln!(self.output, "{answer}").and_then(|()| self.output.flush()) {
            self.failure = Some(failure);
            self.ended = true;
            self.gone.store(true, Ordering::SeqCst);
        }
    }

    fn take(&mut self, server: &mut dyn ToolServer, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => match respond(server, &message) {
                Response::Answer(answer) => self.send(&answer),
                Response::Quiet => {}
                Response::Wait(waiting) => self.waiting.push(waiting),
            },
            Err(_) => self.send(&error(&Value::Null, -32700, "Parse error")),
        }
    }

    fn receive(&mut self, incoming: Incoming) {
        if let Incoming::End(failure) = incoming {
            self.ended = true;
            self.failure = self.failure.take().or(failure);
        }
    }

    /// Call the waiting calls again and send the answers that came; with `settle` (the work
    /// they waited for ended) answer every one.
    fn retry(&mut self, server: &mut dyn ToolServer, settle: bool) {
        let now = Instant::now();
        for mut call in std::mem::take(&mut self.waiting) {
            if let Some(until) = call.until {
                let left = until.saturating_duration_since(now).as_millis() as u64;
                call.arguments["wait_ms"] = left.into();
            }
            match server.call(&call.name, call.arguments.clone()) {
                Err(Error::Waiting) if !settle => self.waiting.push(call),
                Err(Error::Waiting) => self.send(&tool_reply(
                    &call.id,
                    Err(Error::msg(
                        "The background run this call waited for is gone.",
                    )),
                )),
                result => self.send(&tool_reply(&call.id, result)),
            }
        }
    }
}

impl<W: Write> Pump for Link<W> {
    fn pump(&mut self, server: &mut dyn ToolServer, wait: Duration) {
        let until = Instant::now() + wait;
        loop {
            self.retry(server, false);
            let left = until.saturating_duration_since(Instant::now());
            if self.ended {
                thread::sleep(left);
                return;
            }
            match self.lines.recv_timeout(left) {
                Ok(Incoming::Line(line)) => self.take(server, &line),
                Ok(incoming) => self.receive(incoming),
                Err(RecvTimeoutError::Timeout) => return,
                Err(RecvTimeoutError::Disconnected) => self.ended = true,
            }
            if Instant::now() >= until {
                self.retry(server, false);
                return;
            }
        }
    }
}

/// Serve until `input` ends, then close the session (releasing held input). A reader thread
/// passes the input on, so messages that arrive while a background run works are answered
/// at its wait points ([`Pump`]); once the input ends, running work is told the client is
/// gone.
pub fn serve(
    input: impl BufRead + Send + 'static,
    output: impl Write,
    server: &mut dyn ToolServer,
) -> io::Result<()> {
    let gone = Arc::new(AtomicBool::new(false));
    server.attach_client(Arc::clone(&gone));
    let (send, lines) = mpsc::channel();
    let reader_gone = Arc::clone(&gone);
    thread::spawn(move || {
        let mut failure = None;
        for line in input.lines() {
            match line {
                Ok(line) => {
                    if send.send(Incoming::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        reader_gone.store(true, Ordering::SeqCst);
        let _ = send.send(Incoming::End(failure));
    });
    let mut link = Link {
        lines,
        output,
        waiting: Vec::new(),
        ended: false,
        failure: None,
        gone,
    };
    while !link.ended {
        match link.lines.recv() {
            Ok(Incoming::Line(line)) => link.take(server, &line),
            Ok(incoming) => link.receive(incoming),
            Err(_) => link.ended = true,
        }
        server.work(&mut link);
        link.retry(server, true);
    }
    server.close();
    link.failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[derive(Default)]
    struct Recorder {
        calls: Vec<(String, Value)>,
        closed: bool,
    }

    impl ToolServer for Recorder {
        fn tools(&self) -> Vec<Value> {
            ["game_status", "game_crash", "frame_capture"]
                .map(|name| json!({ "name": name }))
                .to_vec()
        }

        fn offers(&self, name: &str) -> bool {
            self.tools().iter().any(|tool| tool["name"] == name)
        }

        fn call(&mut self, name: &str, arguments: Value) -> Result<Vec<Content>> {
            self.calls.push((name.to_owned(), arguments));
            match name {
                "game_crash" => Err(Error::msg("Minecraft is not running")),
                "frame_capture" => Ok(vec![
                    Content::Png(vec![1, 2, 3]),
                    Content::Text("{}".into()),
                ]),
                _ => Ok(vec![Content::Text("ok".into())]),
            }
        }

        fn close(&mut self) {
            self.closed = true;
        }
    }

    fn call(server: &mut Recorder, name: &str, arguments: Value) -> Value {
        handle(
            server,
            &json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }),
        )
        .unwrap()
    }

    #[test]
    fn initialize_negotiates_a_supported_version() {
        let mut server = Recorder::default();
        let ask = |server: &mut Recorder, version: &str| {
            handle(server, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": version}}))
                .unwrap()["result"]["protocolVersion"]
                .clone()
        };
        assert_eq!(ask(&mut server, "2024-11-05"), "2024-11-05");
        assert_eq!(ask(&mut server, "1999-01-01"), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn prompts_are_offered_listed_and_filled_in() {
        let mut server = Recorder::default();
        let mut ask = |method: &str, params: Value| {
            handle(
                &mut server,
                &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
            )
            .unwrap()
        };
        let init = ask("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(init["result"]["capabilities"]["prompts"], json!({}));
        let listed = ask("prompts/list", json!({}));
        assert_eq!(listed["result"]["prompts"], json!(prompts::list()));
        let got = ask(
            "prompts/get",
            json!({"name": "debug_pack_errors", "arguments": {"pack": "/p", "world": "w"}}),
        );
        let text = got["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("pack_validate {path: \"/p\"}"));
        let missing = ask("prompts/get", json!({"name": "debug_pack_errors"}));
        assert_eq!(missing["error"]["code"], -32602);
        let unknown = ask("prompts/get", json!({"name": "nope"}));
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_an_error() {
        let mut server = Recorder::default();
        assert!(
            handle(
                &mut server,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );
        let reply = handle(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": "a", "method": "resources/list"}),
        )
        .unwrap();
        assert_eq!(reply["error"]["code"], -32601);
        assert_eq!(reply["id"], "a");
        let ping = handle(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
        )
        .unwrap();
        assert_eq!(ping["result"], json!({}));
    }

    #[test]
    fn only_listed_tools_are_dispatched() {
        let mut server = Recorder::default();
        assert_eq!(
            call(&mut server, "unknown_tool", json!({}))["error"]["code"],
            -32602
        );
        assert_eq!(
            call(&mut server, "game_status", json!([1]))["error"]["code"],
            -32602
        );
        assert!(server.calls.is_empty());
        let reply = call(&mut server, "game_status", Value::Null);
        assert_eq!(reply["result"]["content"][0]["text"], "ok");
        assert_eq!(server.calls, [("game_status".to_owned(), json!({}))]);
    }

    #[test]
    fn tool_failures_are_tool_errors_not_protocol_errors() {
        let mut server = Recorder::default();
        let reply = call(&mut server, "game_crash", json!({"kind": "panic"}));
        assert_eq!(reply["result"]["isError"], true);
        let text: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["error"], "Minecraft is not running");
    }

    #[test]
    fn images_are_base64_png_content() {
        let mut server = Recorder::default();
        let reply = call(&mut server, "frame_capture", json!({}));
        assert_eq!(
            reply["result"]["content"][0],
            json!({"type": "image", "data": "AQID", "mimeType": "image/png"})
        );
    }

    #[test]
    fn serve_answers_each_line_and_closes_the_session_at_eof() {
        let mut server = Recorder::default();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            "\n",
            "\n",
            "not json\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
        );
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, &mut server).unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["result"]["tools"], json!(server.tools()));
        assert_eq!(lines[1]["error"]["code"], -32700);
        assert!(server.closed);
    }

    /// A server whose `start` queues work that runs for up to `steps` waits, answering
    /// `status` with how far it got and `wait` once it ended.
    #[derive(Default)]
    struct Worker {
        queued: bool,
        running: bool,
        step: u32,
        steps: u32,
        /// Why the work ended.
        ended: Option<&'static str>,
        gone: Option<Arc<AtomicBool>>,
        closed: bool,
    }

    impl ToolServer for Worker {
        fn tools(&self) -> Vec<Value> {
            ["start", "status", "wait"]
                .map(|name| json!({ "name": name }))
                .to_vec()
        }

        fn offers(&self, name: &str) -> bool {
            self.tools().iter().any(|tool| tool["name"] == name)
        }

        fn call(&mut self, name: &str, _: Value) -> Result<Vec<Content>> {
            Ok(vec![Content::Text(match name {
                "start" => {
                    self.queued = true;
                    "started".into()
                }
                "status" if self.running => format!("running at step {}", self.step),
                "status" => "idle".into(),
                "wait" if self.running => return Err(Error::Waiting),
                _ => format!("ended: {}", self.ended.unwrap_or("never ran")),
            })])
        }

        fn close(&mut self) {
            self.closed = true;
        }

        fn work(&mut self, pump: &mut dyn Pump) {
            if !std::mem::take(&mut self.queued) {
                return;
            }
            self.running = true;
            self.ended = Some("done");
            for step in 0..self.steps {
                self.step = step;
                pump.pump(self, Duration::from_millis(20));
                if self
                    .gone
                    .as_ref()
                    .is_some_and(|gone| gone.load(Ordering::SeqCst))
                {
                    self.ended = Some("client gone");
                    break;
                }
            }
            self.running = false;
        }

        fn attach_client(&mut self, gone: Arc<AtomicBool>) {
            self.gone = Some(gone);
        }
    }

    #[test]
    fn calls_are_answered_while_queued_work_runs_and_the_work_stops_when_the_client_goes() {
        let mut server = Worker {
            steps: 500,
            ..Worker::default()
        };
        let call = |id: u32, name: &str| {
            format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name}})
            )
        };
        let input = [call(1, "start"), call(2, "status"), call(3, "wait")].concat();
        let started = Instant::now();
        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output, &mut server).unwrap();
        let texts: Vec<(Value, String)> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| {
                let answer: Value = serde_json::from_str(line).unwrap();
                let text = answer["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                (answer["id"].clone(), text)
            })
            .collect();
        assert_eq!(
            texts,
            [
                (json!(1), "started".to_owned()),
                (json!(2), "running at step 0".to_owned()),
                (json!(3), "ended: client gone".to_owned()),
            ]
        );
        // The input ended, so the work stopped long before its 500 steps.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(server.closed);
    }
}
