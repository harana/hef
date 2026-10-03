//! Which encodings a file of a given format version may legally carry.
//!
//! Every pipeline family has an availability window: the format version that introduced it and, once retired, the
//! version that retired it — retirement closes the writer, never the reader, so old files stay readable forever. A
//! block whose recorded pipeline lies outside its window for the file's declared version could not legally have been
//! written by that version, so validation treats it as corruption or forgery and refuses to decode it. This
//! generalizes the single ad-hoc rule for the retired Vortex shredded transform into a mechanical check over every
//! family.
//!
//! See: hef-reader-compatibility/spec.md

use crate::error::FormatError;

/// One pipeline family's availability window, keyed by the transform discriminant in the pipeline id's low byte.
/// `retired` names the first format version that may no longer write the family; `None` means it is still writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailabilityWindow {
    pub introduced: (u16, u16),
    pub retired: Option<(u16, u16)>,
    pub transform_id: u32,
}

/// The pinned windows for every transform family the format has ever assigned. Discriminant 11 (the removed Vortex
/// shredded-block serialization) is recorded as retired at the version that introduced it, so no declared version may
/// legally carry it — the same outcome as the reader's unknown-transform rejection, now stated as a window.
pub const PINNED_AVAILABILITY_WINDOWS: [AvailabilityWindow; 14] = [
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 0,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 1,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 2,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 3,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 4,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 5,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 6,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 7,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 8,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 9,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 10,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: Some((1, 0)),
        transform_id: 11,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 12,
    },
    AvailabilityWindow {
        introduced: (1, 0),
        retired: None,
        transform_id: 13,
    },
];

/// Checks one recorded pipeline against `windows` for a file declaring `format_version`: the transform must have a
/// window, the file's version must be at or past its introduction, and — when the family is retired — before its
/// retirement. Anything else refuses: a pipeline the declared version could not legally have written is corruption or
/// forgery, never something to decode.
pub fn validate_pipeline_window(
    pipeline_id: u32,
    format_version: (u16, u16),
    windows: &[AvailabilityWindow],
) -> Result<(), FormatError> {
    let transform_id = pipeline_id & 0xFF;
    let Some(window) = windows.iter().find(|window| window.transform_id == transform_id) else {
        return Err(FormatError::Structural {
            rule: "pipeline transform has no availability window",
        });
    };
    if format_version < window.introduced {
        return Err(FormatError::Structural {
            rule: "pipeline introduced after the file's declared format version",
        });
    }
    if window.retired.is_some_and(|retired| format_version >= retired) {
        return Err(FormatError::Structural {
            rule: "pipeline retired at or before the file's declared format version",
        });
    }
    Ok(())
}

/// [`validate_pipeline_window`] against the pinned windows — the check the reader and conformance run.
pub fn validate_pipeline_window_pinned(pipeline_id: u32, format_version: (u16, u16)) -> Result<(), FormatError> {
    validate_pipeline_window(pipeline_id, format_version, &PINNED_AVAILABILITY_WINDOWS)
}

#[cfg(test)]
#[path = "test/availability.rs"]
mod tests;
