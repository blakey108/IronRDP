//! RemoteFX (MS-RDPRFX) decoding.
//!
//! [`TileDecoder`] turns one encoded `TS_RFX_TILE` into 64x64 RGBA pixels (RLGR entropy decoding,
//! sub-band reconstruction, dequantization, inverse DWT and YCbCr to RGB conversion).
//! [`MessageDecoder`] walks a whole RemoteFX message, as carried in an MS-RDPEGFX
//! `RDPGFX_CODECID_CAVIDEO` command, and returns its region rectangles and decoded tiles.

use ironrdp_core::{DecodeError, ReadCursor, decode_cursor};
use ironrdp_pdu::codecs::rfx::{Block, CodecChannel, EntropyAlgorithm, Quant, RfxRectangle, Tile};

use crate::color_conversion::{self, YCbCrBuffer};
use crate::rlgr::{self, RlgrError};
use crate::{dwt, quantization, subband_reconstruction};

/// Edge of a RemoteFX tile, in pixels.
pub const TILE_SIZE: u16 = 64;

const TILE_PIXELS: usize = 64 * 64;

/// Byte length of one decoded RGBA tile.
pub const TILE_RGBA_LEN: usize = TILE_PIXELS * 4;

/// Error type for RemoteFX decoding.
#[derive(Debug)]
pub enum RfxDecodeError {
    /// A RemoteFX block failed to parse.
    Pdu(DecodeError),
    /// RLGR decoding of a tile component failed.
    Rlgr(RlgrError),
    /// The YCbCr to RGB conversion failed.
    ColorConversion(std::io::Error),
    /// A tile references a quantization table entry the tileset does not carry.
    InvalidQuantIndex { index: u8, table_len: usize },
    /// The message lacks a block it needs to be rendered.
    MissingBlock(&'static str),
}

impl core::fmt::Display for RfxDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pdu(_) => write!(f, "RemoteFX block decode"),
            Self::Rlgr(_) => write!(f, "RemoteFX RLGR decode"),
            Self::ColorConversion(_) => write!(f, "RemoteFX color conversion"),
            Self::InvalidQuantIndex { index, table_len } => {
                write!(f, "quant index {index} exceeds table length {table_len}")
            }
            Self::MissingBlock(name) => write!(f, "RemoteFX message missing {name} block"),
        }
    }
}

impl core::error::Error for RfxDecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Pdu(e) => Some(e),
            Self::Rlgr(e) => Some(e),
            Self::ColorConversion(e) => Some(e),
            Self::InvalidQuantIndex { .. } | Self::MissingBlock(_) => None,
        }
    }
}

impl From<DecodeError> for RfxDecodeError {
    fn from(e: DecodeError) -> Self {
        Self::Pdu(e)
    }
}

impl From<RlgrError> for RfxDecodeError {
    fn from(e: RlgrError) -> Self {
        Self::Rlgr(e)
    }
}

/// Decodes RemoteFX tiles, reusing its scratch buffers from one tile to the next.
#[derive(Debug, Clone)]
pub struct TileDecoder {
    ycbcr: [Vec<i16>; 3],
    temp: Vec<i16>,
    output: Vec<u8>,
}

impl Default for TileDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TileDecoder {
    pub fn new() -> Self {
        Self {
            ycbcr: [vec![0; TILE_PIXELS], vec![0; TILE_PIXELS], vec![0; TILE_PIXELS]],
            temp: vec![0; TILE_PIXELS],
            output: vec![0; TILE_RGBA_LEN],
        }
    }

    /// Decode one tile into 64x64 RGBA pixels, row-major.
    ///
    /// The returned slice borrows the decoder's output buffer and is overwritten by the next call.
    pub fn decode(
        &mut self,
        tile: &Tile<'_>,
        quants: &[Quant],
        entropy_algorithm: EntropyAlgorithm,
    ) -> Result<&[u8], RfxDecodeError> {
        let quant = |index: u8| {
            quants.get(usize::from(index)).ok_or(RfxDecodeError::InvalidQuantIndex {
                index,
                table_len: quants.len(),
            })
        };
        let components = [
            (quant(tile.y_quant_index)?, tile.y_data),
            (quant(tile.cb_quant_index)?, tile.cb_data),
            (quant(tile.cr_quant_index)?, tile.cr_data),
        ];

        for ((quant, data), output) in components.into_iter().zip(self.ycbcr.iter_mut()) {
            rlgr::decode(entropy_algorithm, data, output)?;
            subband_reconstruction::decode(&mut output[4032..]);
            quantization::decode(output, quant);
            dwt::decode(output, &mut self.temp);
        }

        let ycbcr = YCbCrBuffer {
            y: &self.ycbcr[0],
            cb: &self.ycbcr[1],
            cr: &self.ycbcr[2],
        };
        color_conversion::ycbcr_to_rgba(ycbcr, &mut self.output).map_err(RfxDecodeError::ColorConversion)?;

        Ok(&self.output)
    }
}

/// One decoded tile of a RemoteFX message.
#[derive(Debug, Clone)]
pub struct DecodedTile {
    /// Tile column; the tile's left edge is `x_idx * 64` from the message origin.
    pub x_idx: u16,
    /// Tile row; the tile's top edge is `y_idx * 64` from the message origin.
    pub y_idx: u16,
    /// RGBA pixels, 64x64, row-major ([`TILE_RGBA_LEN`] bytes).
    pub pixels: Vec<u8>,
}

/// The renderable content of one RemoteFX message.
#[derive(Debug, Clone, Default)]
pub struct DecodedMessage {
    /// Region rectangles relative to the message origin. Only the parts of a tile that fall within
    /// one of them are valid. Empty when the message carried no rectangle.
    pub rectangles: Vec<RfxRectangle>,
    /// The decoded tiles.
    pub tiles: Vec<DecodedTile>,
}

/// Decodes complete RemoteFX messages (MS-RDPRFX 2.2.2), as carried in an MS-RDPEGFX
/// `RDPGFX_CODECID_CAVIDEO` command.
///
/// The header blocks (sync, codec versions, channels, context) may appear in any message and in any
/// order; they carry nothing a tile decode needs, because the tileset names its own entropy
/// algorithm, so they are accepted and skipped.
#[derive(Debug, Clone, Default)]
pub struct MessageDecoder {
    tiles: TileDecoder,
}

impl MessageDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode every tile of `data`, a sequence of RemoteFX blocks.
    pub fn decode(&mut self, data: &[u8]) -> Result<DecodedMessage, RfxDecodeError> {
        let mut src = ReadCursor::new(data);
        let mut message = DecodedMessage::default();
        let mut frame_begun = false;
        let mut tileset_seen = false;

        while !src.is_empty() {
            match decode_cursor::<Block<'_>>(&mut src)? {
                Block::CodecChannel(CodecChannel::FrameBegin(_)) => frame_begun = true,
                Block::CodecChannel(CodecChannel::Region(region)) => {
                    message.rectangles.extend(region.rectangles);
                }
                Block::CodecChannel(CodecChannel::TileSet(tile_set)) => {
                    tileset_seen = true;
                    message.tiles.reserve(tile_set.tiles.len());
                    for tile in &tile_set.tiles {
                        let pixels = self
                            .tiles
                            .decode(tile, &tile_set.quants, tile_set.entropy_algorithm)?
                            .to_vec();
                        message.tiles.push(DecodedTile {
                            x_idx: tile.x,
                            y_idx: tile.y,
                            pixels,
                        });
                    }
                }
                Block::CodecChannel(CodecChannel::FrameEnd(_)) => break,
                // Header and capability blocks: nothing a decode depends on.
                Block::Sync(_)
                | Block::CodecVersions(_)
                | Block::Channels(_)
                | Block::CodecChannel(CodecChannel::Context(_))
                | Block::Caps(_)
                | Block::CapabilitySet(_)
                | Block::Tile(_) => {}
            }
        }

        if !frame_begun {
            return Err(RfxDecodeError::MissingBlock("FRAME_BEGIN"));
        }
        if !tileset_seen {
            return Err(RfxDecodeError::MissingBlock("TILESET"));
        }

        Ok(message)
    }
}
