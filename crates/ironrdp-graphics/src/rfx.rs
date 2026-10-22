//! RemoteFX ([MS-RDPRFX]) decoding.
//!
//! [`TileDecoder`] turns one encoded `TS_RFX_TILE` into 64x64 RGBA pixels (RLGR entropy decoding,
//! sub-band reconstruction, dequantization, inverse DWT and YCbCr to RGB conversion, [3.1.8]).
//! [`MessageDecoder`] parses a whole RemoteFX message ([2.2.2]), as carried in an [MS-RDPEGFX]
//! `RDPGFX_CODECID_CAVIDEO` command, into its region rectangles and still-encoded tiles, so the
//! caller decodes only the tiles it will draw.
//!
//! [MS-RDPRFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdprfx/62495a4a-a495-46ea-b459-5cde04c44549
//! [2.2.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdprfx/edd6c368-4f39-420a-bf14-a0e83c654ab7
//! [3.1.8]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdprfx/e5afdc95-00bf-46f9-adea-6c641b54af26
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use ironrdp_core::{DecodeError, ReadCursor, decode_cursor};
use ironrdp_pdu::codecs::rfx::{Block, CodecChannel, EntropyAlgorithm, Quant, RfxRectangle, Tile, TileSetPdu};

use crate::color_conversion::{self, YCbCrBuffer};
use crate::rlgr::{self, RlgrError};
use crate::{dwt, quantization, subband_reconstruction};

/// Edge of a RemoteFX tile, in pixels.
pub const TILE_SIZE: u16 = 64;

const TILE_PIXELS: usize = 64 * 64;

/// Byte length of one decoded RGBA tile.
pub const TILE_RGBA_LEN: usize = TILE_PIXELS * 4;

/// The most tiles one message may carry, across all its tilesets.
///
/// `numTiles` is a 16-bit field and a message may hold several tilesets, so a small payload can
/// name far more tiles than any surface has. 16,384 tiles cover 8192x8192 pixels, more than an
/// 8K display (7680x4320, 8,100 tiles).
pub const MAX_TILES_PER_MESSAGE: usize = 16384;

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
    /// A tileset names a different entropy algorithm than the stream's `TS_RFX_CONTEXT`.
    EntropyMismatch {
        context: EntropyAlgorithm,
        tileset: EntropyAlgorithm,
    },
    /// The message carries more than [`MAX_TILES_PER_MESSAGE`] tiles.
    TooManyTiles(usize),
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
            Self::EntropyMismatch { context, tileset } => {
                write!(
                    f,
                    "tileset entropy {tileset:?} differs from context entropy {context:?}"
                )
            }
            Self::TooManyTiles(count) => {
                write!(
                    f,
                    "RemoteFX message carries {count} tiles, limit is {MAX_TILES_PER_MESSAGE}"
                )
            }
        }
    }
}

impl core::error::Error for RfxDecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Pdu(e) => Some(e),
            Self::Rlgr(e) => Some(e),
            Self::ColorConversion(e) => Some(e),
            Self::InvalidQuantIndex { .. }
            | Self::MissingBlock(_)
            | Self::EntropyMismatch { .. }
            | Self::TooManyTiles(_) => None,
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

/// The renderable content of one RemoteFX message, with its tiles still encoded.
#[derive(Debug, Clone, Default)]
pub struct Message<'a> {
    /// Region rectangles relative to the message origin. Only the parts of a tile that fall within
    /// one of them are valid. Empty when the message carried no rectangle.
    pub rectangles: Vec<RfxRectangle>,
    /// The tilesets, each with its quantization tables and entropy algorithm.
    pub tile_sets: Vec<TileSetPdu<'a>>,
}

impl Message<'_> {
    /// Total tile count across all tilesets.
    pub fn tile_count(&self) -> usize {
        self.tile_sets.iter().map(|tile_set| tile_set.tiles.len()).sum()
    }
}

/// Parses RemoteFX messages ([MS-RDPRFX] [2.2.2]), as carried in an [MS-RDPEGFX]
/// `RDPGFX_CODECID_CAVIDEO` command.
///
/// The block decoders reject a sync, codec-version or context block naming a version or encoding
/// property the codec does not support. The header blocks may come once per stream rather than
/// in every message, so the decoder keeps the entropy algorithm of the last `TS_RFX_CONTEXT` and
/// rejects a tileset that disagrees with it.
///
/// [MS-RDPRFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdprfx/62495a4a-a495-46ea-b459-5cde04c44549
/// [2.2.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdprfx/edd6c368-4f39-420a-bf14-a0e83c654ab7
/// [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
#[derive(Debug, Clone, Default)]
pub struct MessageDecoder {
    context_entropy: Option<EntropyAlgorithm>,
}

impl MessageDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `data`, a sequence of RemoteFX blocks, without decoding any tile.
    pub fn parse<'a>(&mut self, data: &'a [u8]) -> Result<Message<'a>, RfxDecodeError> {
        let mut src = ReadCursor::new(data);
        let mut message = Message::default();
        let mut frame_begun = false;
        let mut tile_count = 0usize;

        while !src.is_empty() {
            match decode_cursor::<Block<'a>>(&mut src)? {
                Block::CodecChannel(CodecChannel::Context(context)) => {
                    self.context_entropy = Some(context.entropy_algorithm);
                }
                Block::CodecChannel(CodecChannel::FrameBegin(_)) => frame_begun = true,
                Block::CodecChannel(CodecChannel::Region(region)) => {
                    message.rectangles.extend(region.rectangles);
                }
                Block::CodecChannel(CodecChannel::TileSet(tile_set)) => {
                    if let Some(context) = self.context_entropy {
                        if context != tile_set.entropy_algorithm {
                            return Err(RfxDecodeError::EntropyMismatch {
                                context,
                                tileset: tile_set.entropy_algorithm,
                            });
                        }
                    }
                    tile_count = tile_count.saturating_add(tile_set.tiles.len());
                    if tile_count > MAX_TILES_PER_MESSAGE {
                        return Err(RfxDecodeError::TooManyTiles(tile_count));
                    }
                    message.tile_sets.push(tile_set);
                }
                Block::CodecChannel(CodecChannel::FrameEnd(_)) => break,
                // Validated by their block decoders; nothing else to take from them.
                Block::Sync(_) | Block::CodecVersions(_) | Block::Channels(_) => {}
                // Capability blocks belong to the capability exchange, and a bare tile only
                // occurs inside a tileset.
                Block::Caps(_) | Block::CapabilitySet(_) | Block::Tile(_) => {}
            }
        }

        if !frame_begun {
            return Err(RfxDecodeError::MissingBlock("FRAME_BEGIN"));
        }
        if message.tile_sets.is_empty() {
            return Err(RfxDecodeError::MissingBlock("TILESET"));
        }

        Ok(message)
    }
}
