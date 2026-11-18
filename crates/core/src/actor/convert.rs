use super::*;

/// The contract code for an engine failure to open or play a file.
pub(super) fn engine_error(e: &EngineError) -> ContractError {
    let code = match e {
        EngineError::Source(mpmp_media_io::Error::Open { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            ErrorCode::FileMissing
        }
        EngineError::Source(mpmp_media_io::Error::Io(source))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            ErrorCode::FileMissing
        }
        EngineError::Source(_) => ErrorCode::OpenFailed,
        EngineError::Unsupported(_) => ErrorCode::UnsupportedFormat,
        EngineError::Decode(_) => ErrorCode::DecodeError,
        EngineError::Output(_) => ErrorCode::OutputFailed,
    };
    ContractError::failed(code, e.to_string())
}

/// The engine reports failures as text (its `EngineError` display): the code, best
/// effort.
pub(super) fn engine_failure_code(reason: &str) -> ErrorCode {
    if reason.starts_with("cannot open") {
        ErrorCode::OpenFailed
    } else if reason.starts_with("unsupported") || reason.starts_with("not supported") {
        ErrorCode::UnsupportedFormat
    } else {
        ErrorCode::DecodeError
    }
}

pub(super) fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub(super) fn convert_signal_path(path: &SignalPath) -> SignalPathInfo {
    SignalPathInfo {
        share_mode: Some(match path.share_mode {
            mpmp_engine::output::ShareMode::Shared => crate::contract::OutputShareMode::Shared,
            mpmp_engine::output::ShareMode::Exclusive => {
                crate::contract::OutputShareMode::Exclusive
            }
        }),
        output_device_id: None,
        backend: Some(crate::contract::OutputBackend::Cpal),
        sample_integrity: Some(crate::contract::SampleIntegrity::Unverified),
        codec: path.source.codec.to_lowercase(),
        lossless: path.source.lossless,
        source_rate: path.source.sample_rate,
        source_channels: path.source.channels,
        source_bits_per_sample: path.source.bits_per_sample,
        stages: path
            .stages
            .iter()
            .map(|s| StageInfo {
                name: s.name.to_owned(),
                detail: s.detail.clone(),
                alters_samples: s.alters_samples,
                kind: match s.kind {
                    mpmp_engine::StageKind::Resample => SignalStageKind::Resample,
                    mpmp_engine::StageKind::Channels => SignalStageKind::Channels,
                    mpmp_engine::StageKind::WordLength => SignalStageKind::WordLength,
                },
                input_format: Some(SignalFormat {
                    sample_rate: path.source.sample_rate,
                    channels: path.source.channels,
                    bits_per_sample: path.source.bits_per_sample,
                    valid_bits_per_sample: path.source.bits_per_sample,
                    sample_format: None,
                }),
                output_format: Some(SignalFormat {
                    sample_rate: path.output.sample_rate,
                    channels: path.output.channels,
                    bits_per_sample: Some(path.output.sample_type.bits()),
                    valid_bits_per_sample: (path.output.sample_type
                        != mpmp_engine::output::SampleType::F32)
                        .then_some(path.output.sample_type.bits()),
                    sample_format: Some(match path.output.sample_type {
                        mpmp_engine::output::SampleType::F32 => SampleFormat::F32,
                        mpmp_engine::output::SampleType::I16 => SampleFormat::I16,
                        mpmp_engine::output::SampleType::I32 => SampleFormat::I32,
                    }),
                }),
                ..Default::default()
            })
            .collect(),
        output_rate: path.output.sample_rate,
        output_channels: path.output.channels,
        output_format: match path.output.sample_type {
            mpmp_engine::output::SampleType::F32 => SampleFormat::F32,
            mpmp_engine::output::SampleType::I16 => SampleFormat::I16,
            mpmp_engine::output::SampleType::I32 => SampleFormat::I32,
        },
        reason_codes: match &path.bit_perfect {
            BitPerfect::Yes => Vec::new(),
            BitPerfect::Unknown(_) => vec![SignalReasonCode::SharedMixer],
            BitPerfect::No(_) => {
                let mut codes = Vec::new();
                if !path.source.lossless {
                    codes.push(SignalReasonCode::LossySource);
                }
                codes.extend(path.stages.iter().filter(|s| s.alters_samples).map(
                    |s| match s.kind {
                        mpmp_engine::StageKind::Resample => SignalReasonCode::Resample,
                        mpmp_engine::StageKind::Channels => SignalReasonCode::Channels,
                        mpmp_engine::StageKind::WordLength => SignalReasonCode::WordLength,
                    },
                ));
                codes
            }
        },
        bit_perfect: match &path.bit_perfect {
            BitPerfect::Yes => BitPerfectInfo::Yes,
            BitPerfect::No(reasons) => BitPerfectInfo::No {
                reasons: reasons.clone(),
            },
            BitPerfect::Unknown(reason) => BitPerfectInfo::Uncertain {
                reason: reason.clone(),
            },
        },
    }
}
