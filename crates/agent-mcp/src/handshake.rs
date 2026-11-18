//! The first line on a new connection, before any MCP message (docs/design/agent-permissions.md
//!): the bridge presents the agent's token, the adapter answers who it is or why not.
//!
//! ```text
//! bridge → adapter  PLAYER-MCP/1 <token>\n
//! adapter → bridge  OK <agent name>\n            then MCP, both ways
//!          ERR <code> <English explanation>\n   then the adapter closes
//! ```
//!
//! The token never enters an MCP message, so the model never sees it. Neither side writes
//! it to a log or an error message.

use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// The protocol name and version the bridge opens with.
pub const HELLO: &str = "PLAYER-MCP/1";

/// The longest handshake line either side accepts, `\n` included.
pub const MAX_LINE: usize = 512;

/// How long either side waits for the other's handshake line.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Why the adapter refused a connection: the `<code>` of an `ERR` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The first line is not `PLAYER-MCP/1 <token>`.
    BadHandshake,
    /// Another version of the handshake than this adapter speaks.
    UnsupportedVersion,
    /// No registered agent has this token (wrong, renewed, removed, or the permissions
    /// cannot be read).
    InvalidToken,
    /// The adapter serves as many connections as it may.
    Busy,
    /// The adapter is shutting down.
    ShuttingDown,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::BadHandshake => "bad_handshake",
            Refusal::UnsupportedVersion => "unsupported_version",
            Refusal::InvalidToken => "invalid_token",
            Refusal::Busy => "busy",
            Refusal::ShuttingDown => "shutting_down",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        [
            Refusal::BadHandshake,
            Refusal::UnsupportedVersion,
            Refusal::InvalidToken,
            Refusal::Busy,
            Refusal::ShuttingDown,
        ]
        .into_iter()
        .find(|r| r.code() == code)
    }

    /// The English explanation sent with the code.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::BadHandshake => "expected PLAYER-MCP/1 and a token",
            Refusal::UnsupportedVersion => "this player speaks PLAYER-MCP/1",
            Refusal::InvalidToken => "no agent has this token",
            Refusal::Busy => "too many agent connections",
            Refusal::ShuttingDown => "the player is shutting down",
        }
    }
}

/// The bridge's opening line.
pub fn hello_line(token: &str) -> String {
    format!("{HELLO} {token}\n")
}

/// The token in the bridge's opening line (without its `\n`).
pub fn parse_hello(line: &str) -> Result<&str, Refusal> {
    let (hello, token) = line.split_once(' ').ok_or(Refusal::BadHandshake)?;
    if hello != HELLO {
        return Err(if hello.starts_with("PLAYER-MCP/") {
            Refusal::UnsupportedVersion
        } else {
            Refusal::BadHandshake
        });
    }
    // A token is printable ASCII without spaces (core's are `pmpa_` and hex).
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Refusal::BadHandshake);
    }
    Ok(token)
}

/// Overwrites a string that held a token before freeing it. Best effort: copies made
/// elsewhere (a read buffer, the environment) are not reached.
pub fn wipe(text: String) {
    let mut bytes = text.into_bytes();
    bytes.fill(0);
    // Keeps the writes: memory about to be freed is otherwise not worth writing.
    std::hint::black_box(&mut bytes);
}

pub fn ok_line(agent: &str) -> String {
    format!("OK {agent}\n")
}

pub fn err_line(refusal: Refusal) -> String {
    format!("ERR {} {}\n", refusal.code(), refusal.message())
}

/// The adapter's answer, as the bridge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Accepted: the agent the token names.
    Ok { agent: String },
    /// Refused; `code` is kept as sent when this bridge does not know it.
    Err {
        refusal: Option<Refusal>,
        code: String,
        message: String,
    },
}

pub fn parse_answer(line: &str) -> Option<Answer> {
    if let Some(agent) = line.strip_prefix("OK ") {
        return Some(Answer::Ok {
            agent: agent.to_owned(),
        });
    }
    let rest = line.strip_prefix("ERR ")?;
    let (code, message) = rest.split_once(' ').unwrap_or((rest, ""));
    Some(Answer::Err {
        refusal: Refusal::from_code(code),
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

/// Reads one handshake line (without its `\n`): at most [`MAX_LINE`] bytes, UTF-8. Bytes
/// after the line stay in `reader`'s buffer. `None` when the peer closed the connection, the
/// line is too long or not UTF-8.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let limit = MAX_LINE as u64;
    let n = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 || line.last() != Some(&b'\n') {
        return Ok(None);
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8(line).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let token = "pmpa_0123456789abcdef";
        let hello = hello_line(token);
        assert_eq!(parse_hello(hello.trim_end()), Ok(token));
        assert_eq!(
            parse_hello("PLAYER-MCP/2 x"),
            Err(Refusal::UnsupportedVersion)
        );
        assert_eq!(parse_hello("HELLO x"), Err(Refusal::BadHandshake));
        assert_eq!(parse_hello("PLAYER-MCP/1 "), Err(Refusal::BadHandshake));
        assert_eq!(parse_hello("PLAYER-MCP/1 a b"), Err(Refusal::BadHandshake));
        assert_eq!(
            parse_answer(ok_line("claude").trim_end()),
            Some(Answer::Ok {
                agent: "claude".into()
            })
        );
        assert_eq!(
            parse_answer(err_line(Refusal::InvalidToken).trim_end()),
            Some(Answer::Err {
                refusal: Some(Refusal::InvalidToken),
                code: "invalid_token".into(),
                message: Refusal::InvalidToken.message().into(),
            })
        );
        assert_eq!(parse_answer("nonsense"), None);
    }

    #[tokio::test]
    async fn lines_are_bounded() {
        let long = format!("{}\n", "x".repeat(MAX_LINE));
        let mut reader = tokio::io::BufReader::new(long.as_bytes());
        assert_eq!(read_line(&mut reader).await.unwrap(), None);
        let mut reader = tokio::io::BufReader::new(&b"OK a\r\nrest"[..]);
        assert_eq!(
            read_line(&mut reader).await.unwrap().as_deref(),
            Some("OK a")
        );
        let mut rest = String::new();
        reader.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "rest");
    }
}
