use super::*;

/// An idempotency key as core keeps it: the origin's label (its kind and name, not an
/// agent's session: guarantee 13), registration instance, and the client's key.
pub(super) type IdempotencyKey = (String, Option<u64>, String);

pub(super) fn key_reused() -> ContractError {
    ContractError::invalid(
        ErrorCode::IdempotencyKeyReused,
        "this idempotency key was used for another command",
    )
    .with_field("idempotency_key")
}

/// Hash the typed command, including image bytes, without retaining its potentially
/// large payload. Normalize numeric zero to preserve Command's equality for -0.0.
pub(super) fn command_fingerprint(command: &Command) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    fn normalize(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Number(number) if number.as_f64() == Some(0.0) => {
                *number = 0.into();
            }
            serde_json::Value::Object(fields) => fields.values_mut().for_each(normalize),
            serde_json::Value::Array(items) => items.iter_mut().for_each(normalize),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(command).expect("commands serialize");
    normalize(&mut value);
    Sha256::digest(serde_json::to_vec(&value).expect("commands serialize")).into()
}

/// Results of requests with an idempotency key (contract guarantee 13). A result is
/// `None` for a command whose result is never kept (it shows a token): a retry runs it
/// again, but the key still belongs to it.
#[derive(Default)]
pub(super) struct IdempotencyCache {
    pub(super) results: HashMap<IdempotencyKey, ([u8; 32], Option<CommandResult>)>,
    order: VecDeque<(IdempotencyKey, Instant)>,
}

impl IdempotencyCache {
    fn expire(&mut self) {
        let now = Instant::now();
        while let Some((key, at)) = self.order.front() {
            if now.duration_since(*at) < IDEMPOTENCY_TTL && self.order.len() <= IDEMPOTENCY_KEYS {
                break;
            }
            self.results.remove(key);
            self.order.pop_front();
        }
    }

    pub(super) fn get(
        &mut self,
        key: &IdempotencyKey,
    ) -> Option<&([u8; 32], Option<CommandResult>)> {
        self.expire();
        self.results.get(key)
    }

    /// Keeps `result` for retries, unless it failed: a retry with the same key then runs
    /// the command again (a `failed` error may go away; contract guarantee 13). `None`:
    /// only that the key was used for `command`.
    pub(super) fn insert(
        &mut self,
        key: IdempotencyKey,
        command: Command,
        result: Option<CommandResult>,
    ) {
        if result.as_ref().is_some_and(|r| !kept(r)) {
            return;
        }
        if self
            .results
            .insert(key.clone(), (command_fingerprint(&command), result))
            .is_none()
        {
            self.order.push_back((key, Instant::now()));
        }
        self.expire();
    }

    /// The held command of `key` was answered: retries now get this (or run the command
    /// again, if it failed).
    /// Forgets a key: a retry with it runs the command.
    pub(super) fn forget(&mut self, key: &IdempotencyKey) {
        self.results.remove(key);
        self.order.retain(|(k, _)| k != key);
    }

    pub(super) fn update(&mut self, key: &IdempotencyKey, result: CommandResult) {
        if !kept(&result) {
            self.forget(key);
        } else if let Some(entry) = self.results.get_mut(key) {
            entry.1 = Some(result);
        }
    }
}

/// Whether a result is kept for retries: everything but failures (`failed`, or marked
/// `retryable`).
fn kept(result: &CommandResult) -> bool {
    !matches!(result, CommandResult::Err { error }
        if error.kind == ErrorKind::Failed || error.details.retryable)
}
