use super::*;

/// A new agent token: `pmpa_` and 256 random bits in hex.
pub(crate) fn new_token() -> Result<String, ContractError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| {
        ContractError::failed(
            ErrorCode::Io,
            format!("no random numbers from the system: {e}"),
        )
        .retryable()
    })?;
    let mut token = String::with_capacity(5 + 64);
    token.push_str("pmpa_");
    for b in bytes {
        token.push_str(&format!("{b:02x}"));
    }
    Ok(token)
}

pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Compares two hashes in constant time.
pub(super) fn same_hash(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Checks an agent name for `RegisterAgent`: 1–64 letters, digits, `-`, `_`, `.` (so never
/// `:`, which leaves `<platform>:<name>` to adapters that name agents themselves).
pub(crate) fn check_agent_name(name: &str, field: &str) -> Result<(), ContractError> {
    if registered_name(name) {
        Ok(())
    } else {
        Err(ContractError::invalid(
            ErrorCode::InvalidAgentName,
            format!(
                "agent name {name:?}: use 1 to {MAX_AGENT_NAME} letters, digits, '-', '_' or '.'"
            ),
        )
        .with_field(field))
    }
}

/// Checks the agent a grant is for: a registered agent's name, or `<platform>:<name>` for
/// one an adapter names itself (the platform: 1–32 lowercase ASCII letters and digits).
pub(crate) fn check_grant_agent(name: &str, field: &str) -> Result<(), ContractError> {
    let own = |platform: &str, rest: &str| {
        (1..=MAX_PLATFORM).contains(&platform.len())
            && platform
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && registered_name(rest)
    };
    let ok = match name.split_once(':') {
        Some((platform, rest)) => own(platform, rest),
        None => registered_name(name),
    };
    if ok {
        Ok(())
    } else {
        Err(ContractError::invalid(
            ErrorCode::InvalidAgentName,
            format!(
                "agent name {name:?}: use a registered agent's name, or <platform>:<name> \
                 for an agent an adapter names (platform: lowercase letters and digits)"
            ),
        )
        .with_field(field))
    }
}

fn registered_name(name: &str) -> bool {
    let length = name.chars().count();
    (1..=MAX_AGENT_NAME).contains(&length)
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}
