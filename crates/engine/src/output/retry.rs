//! One bounded default-to-endpoint retry, shared by capabilities and opening.
//!
//! 内部共享，供 mpmp-core 与 mpmp-output-cpal 使用，不属于稳定 API。
use crate::output::OutputError;

pub fn retry_on_endpoint<T, D>(
    primary: Result<T, OutputError>,
    resolve: impl FnOnce() -> Result<(D, String), OutputError>,
    retry: impl FnOnce(D) -> Result<T, OutputError>,
    operation: &str,
) -> Result<T, OutputError> {
    let original = match primary {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let (device, endpoint) = resolve().map_err(|error| {
        OutputError::Backend(format!(
            concat!(
                "default {operation} failed: {original}; resolving concrete default ",
                "endpoint failed: {error}",
            ),
            error = error,
            operation = operation,
            original = original,
        ))
    })?;
    tracing::warn!(%original,
     %endpoint,
     operation,
     "default audio handle failed; retrying concrete endpoint");

    retry(device).map_err(|error| {
        OutputError::Backend(format!(
            "default {operation} failed: {original}; concrete endpoint {endpoint} failed: {error}"
        ))
    })
}

/// A fresh apartment is tried before resolving a concrete endpoint.
pub fn retry_on_fresh<T>(
    primary: Result<T, OutputError>,
    retry: impl FnOnce() -> Result<T, OutputError>,
    operation: &str,
) -> Result<T, OutputError> {
    let original = match primary {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    tracing::warn!(%original,
     operation,
     "default audio handle failed; retrying fresh owner thread");

    let result = retry();
    tracing::info!(operation,
     success = result.is_ok(),
     error = ?result.as_ref().err(),
     "fresh owner attempt completed");

    result.map_err(|error| {
        OutputError::Backend(format!(
            "default {operation} failed: {original}; fresh owner failed: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_owner_precedes_endpoint_and_preserves_all_failures() {
        let mut order = Vec::new();
        let fresh = retry_on_fresh(
            Err::<(), _>(OutputError::Backend("original".into())),
            || {
                order.push("fresh");
                Err(OutputError::Backend("fresh".into()))
            },
            "open",
        );
        let result = retry_on_endpoint(
            fresh,
            || Ok(((), "endpoint".into())),
            |_| {
                order.push("endpoint");
                Err(OutputError::Backend("concrete".into()))
            },
            "open",
        );
        assert_eq!(order, ["fresh", "endpoint"]);
        let error = result.unwrap_err().to_string();
        for text in ["original", "fresh", "endpoint", "concrete"] {
            assert!(error.contains(text));
        }
        let result = retry_on_fresh(Err(OutputError::NoDevice), || Ok(42), "open");
        assert_eq!(
            retry_on_endpoint(
                result,
                || -> Result<((), String), _> { panic!("fresh succeeded") },
                |_| unreachable!(),
                "open"
            )
            .unwrap(),
            42
        );
    }
}
