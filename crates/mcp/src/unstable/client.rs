//! Client for the unstable DLL's endpoint: the stable agent's framing and Hello, with the
//! `protocol::unstable` messages.

use std::path::Path;

use protocol::Endpoint;
use protocol::unstable::{ENDPOINT_FILE, PROTOCOL_VERSION, Reply, Request};

use crate::{
    agent::{LineClient, read_endpoint_file},
    error::{Error, Result},
};

/// Anything that answers unstable requests.
pub trait Link {
    fn request(&mut self, request: &Request) -> Result<Reply>;
}

pub struct UnstableConnection(LineClient);

impl UnstableConnection {
    pub fn open(endpoint: &Endpoint) -> Result<Self> {
        let mut connection = Self(LineClient::connect(endpoint.port)?);
        let hello = Request::Hello {
            token: endpoint.token.clone(),
            protocol: PROTOCOL_VERSION,
        };
        match connection.request(&hello)? {
            Reply::Ok => Ok(connection),
            Reply::Error { message } => Err(Error::msg(format!(
                "The unstable layer refused the connection: {message}"
            ))),
            other => Err(Error::msg(format!(
                "The unstable layer answered hello with {other:?}"
            ))),
        }
    }
}

impl Link for UnstableConnection {
    fn request(&mut self, request: &Request) -> Result<Reply> {
        self.0.exchange(request).map_err(|error| match error {
            Error::Disconnected(_) => {
                Error::Disconnected("the unstable layer closed its connection".into())
            }
            other => other,
        })
    }
}

/// Connect to the endpoint named in `dir`, or None when nothing answers there.
pub fn try_connect(dir: &Path) -> Result<Option<UnstableConnection>> {
    let Some(endpoint) = read_endpoint_file(dir, ENDPOINT_FILE) else {
        return Ok(None);
    };
    match UnstableConnection::open(&endpoint) {
        Ok(connection) => Ok(Some(connection)),
        Err(Error::Disconnected(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::fake::serve_lines, testdir::TestDir};

    #[test]
    fn connections_say_hello_with_the_token_and_exchange_lines() {
        let port = serve_lines(|request: Request| match request {
            Request::Hello { token, protocol }
                if token == "secret" && protocol == PROTOCOL_VERSION =>
            {
                Reply::Ok
            }
            Request::Hello { .. } => Reply::Error {
                message: "bad token".into(),
            },
            Request::Screens => Reply::Screens { stacks: vec![] },
            _ => Reply::Error {
                message: "unsupported".into(),
            },
        });
        let dir = TestDir::new("unstable-endpoint");
        let endpoint = |token: &str| Endpoint {
            port,
            token: token.into(),
            pid: 9,
        };
        std::fs::write(
            dir.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint("secret")).unwrap(),
        )
        .unwrap();
        let mut connection = try_connect(dir.path()).unwrap().expect("endpoint answers");
        assert_eq!(
            connection.request(&Request::Screens).unwrap(),
            Reply::Screens { stacks: vec![] }
        );
        std::fs::write(
            dir.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint("wrong")).unwrap(),
        )
        .unwrap();
        assert!(try_connect(dir.path()).is_err());
        assert!(try_connect(&dir.path().join("none")).unwrap().is_none());
    }
}
