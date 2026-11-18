use super::*;

/// Whether `token` looks like a variable reference left unexpanded (Claude Code keeps an
/// unset `${VAR}` as it is): a real token is `pmpa_` and hex digits.
pub(super) fn not_expanded(token: &str) -> bool {
    token.contains(['$', '%', '{'])
}

pub(super) type Reader = BufReader<tokio::io::ReadHalf<ClientStream>>;

pub(super) type Writer = tokio::io::WriteHalf<ClientStream>;

/// Connects and presents the token; the connection once the player accepted it.
pub(super) async fn connect() -> Result<(Reader, Writer), Failure> {
    let endpoints =
        Endpoint::candidates_from_env().map_err(|e| Failure::Endpoint(e.to_string()))?;
    // The variable's own copy is wiped too (best effort: the environment block keeps one).
    let token = match std::env::var(TOKEN_ENV) {
        Ok(raw) => {
            let token = raw.trim().to_owned();
            handshake::wipe(raw);
            token
        }
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(raw)) => {
            wipe_bytes(raw.into_encoded_bytes());
            return Err(Failure::TokenInvalid);
        }
    };
    if token.is_empty() {
        return Err(Failure::TokenMissing);
    }
    if not_expanded(&token) {
        handshake::wipe(token);
        return Err(Failure::TokenNotExpanded);
    }
    let hello = handshake::hello_line(&token);
    handshake::wipe(token);
    if handshake::parse_hello(hello.trim_end()).is_err() || hello.len() > handshake::MAX_LINE {
        handshake::wipe(hello);
        return Err(Failure::TokenInvalid);
    }
    // The player's user is checked before the token is sent (at each endpoint tried).
    let connected = endpoint::connect_any(&endpoints)
        .await
        .map_err(|e| match e {
            ConnectError::NotRunning(_) => Failure::NotRunning,
            ConnectError::NotSameUser => Failure::NotSameUser,
            ConnectError::AccessDenied(_) => Failure::AccessDenied,
            ConnectError::Io(err) => Failure::Connect(err.to_string()),
        });
    let stream = match connected {
        Ok((stream, _)) => stream,
        Err(failure) => {
            handshake::wipe(hello);
            return Err(failure);
        }
    };
    let (read, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let answer = tokio::time::timeout(handshake::TIMEOUT, async {
        // A refusal may come before the hello is read (busy): read the answer regardless.
        let _ = writer.write_all(hello.as_bytes()).await;
        let _ = writer.flush().await;
        handshake::read_line(&mut reader).await
    })
    .await;
    handshake::wipe(hello);
    let line = match answer {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return Err(Failure::NoAnswer),
    };
    match handshake::parse_answer(&line) {
        Some(Answer::Ok { agent }) => {
            eprintln!("player-mcp: connected to mpmp as agent {agent}");
            Ok((reader, writer))
        }
        Some(Answer::Err {
            refusal, message, ..
        }) => Err(match refusal {
            Some(Refusal::InvalidToken) => Failure::TokenInvalid,
            Some(Refusal::Busy) => Failure::Busy,
            Some(Refusal::ShuttingDown) => Failure::NotRunning,
            _ => Failure::Refused(message),
        }),
        None => Err(Failure::NoAnswer),
    }
}

/// Overwrites bytes about to be freed (a token that is not valid UTF-8), like
/// [`handshake::wipe`].
fn wipe_bytes(mut bytes: Vec<u8>) {
    bytes.fill(0);
    std::hint::black_box(&mut bytes);
}
