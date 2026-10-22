//! Alpha codec (MS-RDPEGFX 2.2.4.3 `ALPHACODEC_BITMAP_STREAM`).
//!
//! The stream carries only per-pixel opacity, either raw (one byte per pixel) or run-length encoded
//! with `CLEARCODEC_ALPHA_RLE_SEGMENT` runs (2.2.4.3.1). The color channels of the target come from
//! an earlier command; this codec only replaces their alpha.

use ironrdp_core::ReadCursor;

/// `alphaSig`: the value every stream starts with ("AL", little-endian).
pub const ALPHA_SIGNATURE: u16 = 0x414C;

const HEADER_SIZE: usize = 2 /* alphaSig */ + 2 /* compressed */;

/// Error type for alpha codec decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlphaDecodeError {
    /// The stream is shorter than its header or than the data it announces.
    Truncated,
    /// `alphaSig` is not [`ALPHA_SIGNATURE`].
    InvalidSignature(u16),
    /// The decoded pixel count differs from the bitmap area, which 2.2.4.3 requires to match.
    PixelCountMismatch { expected: usize, decoded: usize },
}

impl core::fmt::Display for AlphaDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "alpha codec stream is truncated"),
            Self::InvalidSignature(signature) => write!(f, "invalid alpha codec signature {signature:#06x}"),
            Self::PixelCountMismatch { expected, decoded } => {
                write!(f, "alpha codec stream encodes {decoded} pixels, bitmap has {expected}")
            }
        }
    }
}

impl core::error::Error for AlphaDecodeError {}

/// Decode an `ALPHACODEC_BITMAP_STREAM` into one opacity byte per pixel, row-major, for a
/// `width` x `height` bitmap.
pub fn decode(data: &[u8], width: u16, height: u16) -> Result<Vec<u8>, AlphaDecodeError> {
    let expected = usize::from(width) * usize::from(height);
    let mut src = ReadCursor::new(data);
    if src.len() < HEADER_SIZE {
        return Err(AlphaDecodeError::Truncated);
    }

    let signature = src.read_u16();
    if signature != ALPHA_SIGNATURE {
        return Err(AlphaDecodeError::InvalidSignature(signature));
    }
    let compressed = src.read_u16();

    if compressed == 0 {
        if src.len() < expected {
            return Err(AlphaDecodeError::Truncated);
        }
        return Ok(src.read_slice(expected).to_vec());
    }

    let mut alpha = Vec::with_capacity(expected);
    while alpha.len() < expected {
        let (value, run_length) = read_segment(&mut src)?;
        let remaining = expected - alpha.len();
        if run_length > remaining {
            return Err(AlphaDecodeError::PixelCountMismatch {
                expected,
                decoded: alpha.len().saturating_add(run_length),
            });
        }
        alpha.resize(alpha.len() + run_length, value);
    }

    Ok(alpha)
}

/// Read one `CLEARCODEC_ALPHA_RLE_SEGMENT` (2.2.4.3.1): a value and its run length, whose
/// encoding widens from one to two to four bytes behind the 0xFF and 0xFFFF escapes.
fn read_segment(src: &mut ReadCursor<'_>) -> Result<(u8, usize), AlphaDecodeError> {
    if src.len() < 2 {
        return Err(AlphaDecodeError::Truncated);
    }
    let value = src.read_u8();
    let factor1 = src.read_u8();
    if factor1 < 0xFF {
        return Ok((value, usize::from(factor1)));
    }

    if src.len() < 2 {
        return Err(AlphaDecodeError::Truncated);
    }
    let factor2 = src.read_u16();
    if factor2 < 0xFFFF {
        return Ok((value, usize::from(factor2)));
    }

    if src.len() < 4 {
        return Err(AlphaDecodeError::Truncated);
    }
    let factor3 = src.read_u32();
    // A run longer than the address space cannot fit any bitmap, so saturate and let the
    // pixel-count check reject it.
    Ok((value, usize::try_from(factor3).unwrap_or(usize::MAX)))
}
