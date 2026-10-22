use ironrdp_core::{Encode as _, WriteCursor};
use ironrdp_graphics::rfx::{MAX_TILES_PER_MESSAGE, MessageDecoder, RfxDecodeError, TILE_RGBA_LEN, TileDecoder};
use ironrdp_graphics::rfx_encode_component;
use ironrdp_pdu::codecs::rfx::{
    Block, CodecChannel, ContextPdu, EntropyAlgorithm, FrameBeginPdu, FrameEndPdu, OperatingMode, Quant, RegionPdu,
    RfxRectangle, Tile, TileSetPdu,
};

/// Encode a constant component: every coefficient set to `value` before the forward transform.
fn flat_component(value: i16, quant: &Quant, entropy: EntropyAlgorithm) -> Vec<u8> {
    let mut input = [value; 4096];
    let mut out = vec![0; 4096];
    let len = rfx_encode_component(&mut input, &mut out, quant, entropy).unwrap();
    out.truncate(len);
    out
}

fn tile(x: u16, y: u16, data: &[u8]) -> Tile<'_> {
    Tile {
        y_quant_index: 0,
        cb_quant_index: 0,
        cr_quant_index: 0,
        x,
        y,
        y_data: data,
        cb_data: data,
        cr_data: data,
    }
}

fn encode_blocks(blocks: &[Block<'_>]) -> Vec<u8> {
    let mut buffer = vec![0; blocks.iter().map(|block| block.size()).sum()];
    let mut cursor = WriteCursor::new(&mut buffer);
    for block in blocks {
        block.encode(&mut cursor).unwrap();
    }
    buffer
}

fn frame(tile_set: TileSetPdu<'_>, rectangles: Vec<RfxRectangle>) -> Vec<u8> {
    encode_blocks(&[
        Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: 7,
            number_of_regions: 1,
        })),
        Block::CodecChannel(CodecChannel::Region(RegionPdu { rectangles })),
        Block::CodecChannel(CodecChannel::TileSet(tile_set)),
        Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu)),
    ])
}

#[test]
fn a_zero_tile_decodes_to_mid_gray() {
    // oracle: spec MS-RDPRFX §3.1.8.2.5 (Y is level-shifted up by 128; zero chroma is gray)
    let quant = Quant::default();
    let zero = flat_component(0, &quant, EntropyAlgorithm::Rlgr3);

    let mut decoder = TileDecoder::new();
    let pixels = decoder
        .decode(&tile(0, 0, &zero), &[quant], EntropyAlgorithm::Rlgr3)
        .unwrap();

    assert_eq!(pixels.len(), TILE_RGBA_LEN);
    for pixel in pixels.chunks_exact(4) {
        assert_eq!(pixel, [128, 128, 128, 255]);
    }
}

#[test]
fn a_tile_naming_a_missing_quant_is_rejected() {
    // oracle: spec MS-RDPRFX §2.2.2.3.4.1 (quantIdx fields index the tileset's quantVals array)
    let quant = Quant::default();
    let zero = flat_component(0, &quant, EntropyAlgorithm::Rlgr3);
    let mut bad = tile(0, 0, &zero);
    bad.cb_quant_index = 2;

    let error = TileDecoder::new()
        .decode(&bad, &[quant], EntropyAlgorithm::Rlgr3)
        .unwrap_err();

    assert!(
        matches!(error, RfxDecodeError::InvalidQuantIndex { index: 2, table_len: 1 }),
        "{error:?}"
    );
}

#[test]
fn a_message_without_headers_parses_its_rectangles_and_tiles() {
    // oracle: spec MS-RDPRFX §2.2.2.3.3 and §2.2.2.3.4 (the region and tileset the test wrote)
    let quant = Quant::default();
    let zero = flat_component(0, &quant, EntropyAlgorithm::Rlgr3);
    let rectangles = vec![
        RfxRectangle {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        },
        RfxRectangle {
            x: 64,
            y: 64,
            width: 10,
            height: 10,
        },
    ];
    let stream = frame(
        TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
            quants: vec![quant],
            tiles: vec![tile(0, 0, &zero), tile(1, 1, &zero)],
        },
        rectangles.clone(),
    );

    let message = MessageDecoder::new().parse(&stream).unwrap();

    assert_eq!(message.rectangles, rectangles);
    assert_eq!(message.tile_count(), 2);
    let positions: Vec<_> = message.tile_sets[0].tiles.iter().map(|tile| (tile.x, tile.y)).collect();
    assert_eq!(positions, [(0, 0), (1, 1)]);
}

#[test]
fn a_tileset_disagreeing_with_the_context_entropy_is_rejected() {
    // oracle: spec MS-RDPRFX §3.1.1 and §3.1.5 (every data message is encoded with the
    // EntropyAlgorithm the TS_RFX_CONTEXT stored)
    let quant = Quant::default();
    let zero = flat_component(0, &quant, EntropyAlgorithm::Rlgr1);
    let mut decoder = MessageDecoder::new();
    let context = encode_blocks(&[
        Block::CodecChannel(CodecChannel::Context(ContextPdu {
            flags: OperatingMode::IMAGE_MODE,
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
        })),
        Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: 0,
            number_of_regions: 1,
        })),
        Block::CodecChannel(CodecChannel::TileSet(TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
            quants: vec![quant.clone()],
            tiles: vec![],
        })),
    ]);
    decoder.parse(&context).unwrap();

    // A later message without headers still follows the context of the stream.
    let stream = frame(
        TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr1,
            quants: vec![quant],
            tiles: vec![tile(0, 0, &zero)],
        },
        vec![],
    );
    let error = decoder.parse(&stream).unwrap_err();

    assert!(
        matches!(
            error,
            RfxDecodeError::EntropyMismatch {
                context: EntropyAlgorithm::Rlgr3,
                tileset: EntropyAlgorithm::Rlgr1
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_message_with_more_tiles_than_any_display_needs_is_rejected_before_decoding() {
    // oracle: input chosen by the test (one tile past the documented 16,384-tile limit)
    let quant = Quant::default();
    let zero = flat_component(0, &quant, EntropyAlgorithm::Rlgr3);
    let tiles: Vec<_> = (0..16_385u32)
        .map(|i| tile(u16::try_from(i % 128).unwrap(), u16::try_from(i / 128).unwrap(), &zero))
        .collect();
    let stream = frame(
        TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
            quants: vec![quant],
            tiles,
        },
        vec![],
    );

    let error = MessageDecoder::new().parse(&stream).unwrap_err();

    assert_eq!(MAX_TILES_PER_MESSAGE, 16_384);
    assert!(matches!(error, RfxDecodeError::TooManyTiles(16_385)), "{error:?}");
}

#[test]
fn a_message_without_a_tileset_is_rejected() {
    // oracle: spec MS-RDPRFX §3.1.8.3.1 (a frame carries a region and a tileset)
    let stream = encode_blocks(&[
        Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: 0,
            number_of_regions: 1,
        })),
        Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu)),
    ]);

    let error = MessageDecoder::new().parse(&stream).unwrap_err();

    assert!(matches!(error, RfxDecodeError::MissingBlock("TILESET")), "{error:?}");
}

#[test]
fn a_message_without_a_frame_begin_is_rejected() {
    // oracle: spec MS-RDPRFX §3.1.8.3.1 (data messages start with TS_RFX_FRAME_BEGIN)
    let stream = encode_blocks(&[Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu))]);

    let error = MessageDecoder::new().parse(&stream).unwrap_err();

    assert!(
        matches!(error, RfxDecodeError::MissingBlock("FRAME_BEGIN")),
        "{error:?}"
    );
}
