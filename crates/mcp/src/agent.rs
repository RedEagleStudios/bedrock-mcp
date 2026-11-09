//! Client for the agent DLL inside the game: JSON lines over loopback TCP.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};

use protocol::{ENDPOINT_FILE, Endpoint, PROTOCOL_VERSION, Reply, Request, Status};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Anything that answers agent requests; the session uses [`AgentConnection`].
pub trait Agent {
    fn request(&mut self, request: &Request) -> Result<Reply>;

    /// Send a request that expects `Ok`.
    fn send(&mut self, request: Request) -> Result<()> {
        match self.request(&request)? {
            Reply::Ok => Ok(()),
            Reply::Error { message } => Err(Error::msg(format!("Developer agent: {message}"))),
            Reply::Status(_) => Err(Error::msg(
                "Developer agent answered with an unexpected status.",
            )),
        }
    }

    fn status(&mut self) -> Result<Status> {
        match self.request(&Request::Status)? {
            Reply::Status(status) => Ok(status),
            Reply::Error { message } => Err(Error::msg(format!("Developer agent: {message}"))),
            Reply::Ok => Err(Error::msg("Developer agent answered status without one.")),
        }
    }
}

/// JSON lines over a loopback TCP connection: one reply line per request line. Shared by
/// the stable agent and the unstable layer.
pub struct LineClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

fn lost(error: impl std::fmt::Display) -> Error {
    Error::Disconnected(format!(
        "Lost the developer agent ({error}); is Minecraft still running?"
    ))
}

impl LineClient {
    /// Connect to `127.0.0.1:port`; failures are [`Error::Disconnected`].
    pub fn connect(port: u16) -> Result<Self> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(lost)?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).map_err(lost)?;
        stream.set_nodelay(true).map_err(lost)?;
        let reader = BufReader::new(stream.try_clone().map_err(lost)?);
        Ok(Self {
            writer: stream,
            reader,
        })
    }

    /// Send one request line and read its reply line.
    pub fn exchange<Q: Serialize, R: DeserializeOwned>(&mut self, request: &Q) -> Result<R> {
        self.writer
            .write_all(protocol::encode(request).as_bytes())
            .map_err(lost)?;
        let mut line = String::new();
        if self.reader.read_line(&mut line).map_err(lost)? == 0 {
            return Err(lost("connection closed"));
        }
        protocol::decode(&line)
            .map_err(|error| Error::msg(format!("Bad reply from the developer agent: {error}")))
    }
}

/// One authenticated connection; the agent drops the previous client when it accepts this one.
pub struct AgentConnection(LineClient);

impl AgentConnection {
    pub fn open(endpoint: &Endpoint) -> Result<Self> {
        let mut connection = Self(LineClient::connect(endpoint.port)?);
        let hello = Request::Hello {
            token: endpoint.token.clone(),
            protocol: PROTOCOL_VERSION,
        };
        match connection.request(&hello)? {
            Reply::Ok => Ok(connection),
            Reply::Error { message } => Err(Error::msg(format!(
                "The developer agent refused the connection: {message}"
            ))),
            Reply::Status(_) => Err(Error::msg(
                "The developer agent answered hello with a status.",
            )),
        }
    }
}

impl Agent for AgentConnection {
    fn request(&mut self, request: &Request) -> Result<Reply> {
        self.0.exchange(request)
    }
}

/// The endpoint file `name` in `dir`.
pub fn read_endpoint_file(dir: &Path, name: &str) -> Option<Endpoint> {
    serde_json::from_slice(&fs::read(dir.join(name)).ok()?).ok()
}

pub fn read_endpoint(dir: &Path) -> Option<Endpoint> {
    read_endpoint_file(dir, ENDPOINT_FILE)
}

/// Connect to the agent named by the endpoint file in `dir`, or None if none answers.
/// A refused token or protocol mismatch is an error, not None.
pub fn try_connect(dir: &Path) -> Result<Option<AgentConnection>> {
    let Some(endpoint) = read_endpoint(dir) else {
        return Ok(None);
    };
    match AgentConnection::open(&endpoint) {
        Ok(connection) => Ok(Some(connection)),
        Err(Error::Disconnected(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;

    use super::*;

    /// Records requests and answers status with a frame counter that advances per poll.
    pub struct FakeAgent {
        pub requests: Vec<Request>,
        pub readings: u64,
        pub disconnect_after: Option<usize>,
        pub replies: VecDeque<Reply>,
    }

    impl FakeAgent {
        pub fn new() -> Self {
            Self {
                requests: Vec::new(),
                readings: 0,
                disconnect_after: None,
                replies: VecDeque::new(),
            }
        }

        /// Requests other than status polls.
        pub fn sent(&self) -> Vec<&Request> {
            self.requests
                .iter()
                .filter(|r| **r != Request::Status)
                .collect()
        }
    }

    impl Agent for FakeAgent {
        fn request(&mut self, request: &Request) -> Result<Reply> {
            if self
                .disconnect_after
                .is_some_and(|limit| self.requests.len() >= limit)
            {
                return Err(lost("closed"));
            }
            self.requests.push(request.clone());
            if let Some(reply) = self.replies.pop_front() {
                return Ok(reply);
            }
            Ok(match request {
                Request::Status => {
                    self.readings += 1;
                    Reply::Status(Status {
                        protocol: PROTOCOL_VERSION,
                        pid: 1,
                        width: 1920,
                        height: 1080,
                        foreground: true,
                        game_active: true,
                        mouse_readings: self.readings,
                        gameinput_versions: vec![],
                        pointer: [0, 0],
                        pointer_override: false,
                    })
                }
                _ => Reply::Ok,
            })
        }
    }

    /// A loopback JSON-lines server answering each request with `handler`; returns its port.
    pub fn serve_lines<Q, R>(handler: impl Fn(Q) -> R + Send + Sync + 'static) -> u16
    where
        Q: DeserializeOwned + 'static,
        R: Serialize + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = std::sync::Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let handler = std::sync::Arc::clone(&handler);
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().unwrap();
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else { return };
                        let Ok(request) = protocol::decode::<Q>(&line) else {
                            return;
                        };
                        let reply = protocol::encode(&handler(request));
                        if writer.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// A stable agent endpoint that accepts any token and answers status.
    pub fn stable_endpoint() -> Endpoint {
        let port = serve_lines(|request: Request| match request {
            Request::Status => FakeAgent::new().request(&Request::Status).unwrap(),
            _ => Reply::Ok,
        });
        Endpoint {
            port,
            token: "t".into(),
            pid: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::BufRead, net::TcpListener, thread};

    use super::*;
    use crate::testdir::TestDir;

    /// A one-connection agent speaking the protocol; returns the requests it saw.
    fn serve(token: &'static str) -> (Endpoint, thread::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut seen = Vec::new();
            for line in BufReader::new(stream).lines() {
                let request: Request = protocol::decode(&line.unwrap()).unwrap();
                let reply = match &request {
                    Request::Hello { token: given, .. } if given != token => Reply::Error {
                        message: "bad token".into(),
                    },
                    Request::Char { .. } => Reply::Error {
                        message: "no window".into(),
                    },
                    _ => Reply::Ok,
                };
                seen.push(request);
                writer
                    .write_all(protocol::encode(&reply).as_bytes())
                    .unwrap();
            }
            seen
        });
        (
            Endpoint {
                port,
                token: "secret".into(),
                pid: 4,
            },
            handle,
        )
    }

    #[test]
    fn connection_authenticates_then_exchanges_one_reply_per_request() {
        let (endpoint, server) = serve("secret");
        let dir = TestDir::new("endpoint");
        fs::write(
            dir.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();
        let mut agent = try_connect(dir.path()).unwrap().expect("agent answers");
        agent.send(Request::Buttons { mask: 1 }).unwrap();
        let error = agent.send(Request::Char { code: 65 }).unwrap_err();
        assert!(matches!(&error, Error::Message(text) if text.contains("no window")));
        drop(agent);
        let seen = server.join().unwrap();
        assert!(matches!(
            seen[0],
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        assert_eq!(seen[1], Request::Buttons { mask: 1 });
    }

    #[test]
    fn wrong_tokens_are_errors_and_missing_agents_are_none() {
        let (mut endpoint, server) = serve("other");
        let dir = TestDir::new("token");
        fs::write(
            dir.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();
        assert!(try_connect(dir.path()).is_err());
        server.join().unwrap();
        // Nobody listens on the port any more.
        endpoint.port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        fs::write(
            dir.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();
        assert!(try_connect(dir.path()).unwrap().is_none());
        assert!(try_connect(&dir.path().join("missing")).unwrap().is_none());
    }
}
