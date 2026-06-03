//! Quality markers for decode and projection paths.

/// Whether decoded data is complete, degraded, or unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeQuality {
    Valid,
    Lossy { reason: String },
    Invalid { reason: String },
}

impl DecodeQuality {
    pub fn lossy(reason: impl Into<String>) -> Self {
        Self::Lossy {
            reason: reason.into(),
        }
    }
}
