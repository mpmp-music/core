//! Retry decisions the core actor relies on, checked against the shared engine helpers.
use mpmp_engine::output::OutputError;
use mpmp_engine::output::retry::*;

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
