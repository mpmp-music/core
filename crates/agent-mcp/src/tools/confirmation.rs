use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    token: String,
    timeout_s: Option<u64>,
}

pub(super) async fn wait_for_confirmation(
    session: &Session,
    args: &JsonObject,
    stop: impl Future<Output = ()>,
) -> Outcome {
    let a: WaitArgs = parse(args)?;
    let timeout = wait_time(a.timeout_s, session.max_wait());
    let token = a.token;
    let status = session.wait(&token, timeout, stop).await;
    let message = |status: &str, message: &str| {
        success(json!({"status": status, "token": token, "message": message}))
    };
    Ok(match status {
        Status::Approved(result) => match *result {
            CommandResult::Ok(receipt) => success(json!({
                "status": "approved", "token": token, "receipt": receipt_value(&receipt),
            })),
            // Approved, but the command failed (refused when checked again, or it failed
            // itself): a failure like any other (the contract error at the top), saying it
            // was approved.
            CommandResult::Err { error } => {
                let mut value = to_value(&error);
                if let Some(map) = value.as_object_mut() {
                    map.insert("status".into(), "approved".into());
                    map.insert("token".into(), token.clone().into());
                }
                failure(value)
            }
            _ => message(
                "unknown",
                concat!(
                    "The answer was missed. Check player_status or recent_actions to see ",
                    "whether it ran.",
                ),
            ),
        },
        Status::Unknown => message(
            "unknown",
            "The answer was missed. Check player_status or recent_actions to see whether it ran.",
        ),
        Status::Rejected => message("rejected", "The user rejected the request; it did not run."),
        Status::Expired => message(
            "expired",
            "Nobody answered in time; it did not run. Ask again if it is still wanted.",
        ),
        Status::Withdrawn => message(
            "withdrawn",
            concat!(
                "The request was withdrawn before it ran (its session ended, or its agent ",
                "was removed); it did not run.",
            ),
        ),
        Status::NotRun => message("not_run", "The user did not approve it; it did not run."),
        Status::Pending => message(
            "pending",
            "No answer yet. Call wait_for_confirmation again to keep waiting.",
        ),
        Status::NotYours => {
            return Err(ContractError::not_found(
                ErrorCode::ConfirmationNotFound,
                "no confirmation with this token was requested in this session",
            )
            .with_field("token"));
        }
    })
}
