use ironrdp_core::{Encode as _, WriteCursor};
use ironrdp_graphics::rfx::{MessageDecoder, RfxDecodeError, TILE_RGBA_LEN, TileDecoder};
use ironrdp_graphics::rfx_encode_component;
use ironrdp_pdu::codecs::rfx::{
    Block, CodecChannel, EntropyAlgorithm, FrameBeginPdu, FrameEndPdu, Quant, RegionPdu, RfxRectangle, Tile, TileSetPdu,
};

/// Encode a constant YCbCr tile: every coefficient of the three components set to `value`
/// before the forward transform, so the decoded tile is one flat color.
fn flat_component(value: i16, quant: &Quant) -> Vec<u8> {
    let mut input = [value; 4096];
    let mut out = vec![0; 4096];
    let len = rfx_encode_component(&mut input, &mut out, quant, EntropyAlgorithm::Rlgr3).unwrap();
    out.truncate(len);
    out
}

fn encode_blocks(blocks: &[Block<'_>]) -> Vec<u8> {
    let mut buffer = vec![0; blocks.iter().map(|block| block.size()).sum()];
    let mut cursor = WriteCursor::new(&mut buffer);
    for block in blocks {
        block.encode(&mut cursor).unwrap();
    }
    buffer
}

#[test]
fn a_zero_tile_decodes_to_mid_gray() {
    let quant = Quant::default();
    let zero = flat_component(0, &quant);
    let tile = Tile {
        y_quant_index: 0,
        cb_quant_index: 0,
        cr_quant_index: 0,
        x: 0,
        y: 0,
        y_data: &zero,
        cb_data: &zero,
        cr_data: &zero,
    };

    let mut decoder = TileDecoder::new();
    let pixels = decoder.decode(&tile, &[quant], EntropyAlgorithm::Rlgr3).unwrap();

    // Y = 0 is the middle of the RemoteFX luma range (MS-RDPRFX 3.1.8.1.3 shifts by 128), and
    // zero chroma is gray.
    assert_eq!(pixels.len(), TILE_RGBA_LEN);
    for pixel in pixels.chunks_exact(4) {
        assert_eq!(pixel, [128, 128, 128, 255]);
    }
}

#[test]
fn a_tile_naming_a_missing_quant_is_rejected() {
    let quant = Quant::default();
    let zero = flat_component(0, &quant);
    let tile = Tile {
        y_quant_index: 0,
        cb_quant_index: 2,
        cr_quant_index: 0,
        x: 0,
        y: 0,
        y_data: &zero,
        cb_data: &zero,
        cr_data: &zero,
    };

    let error = TileDecoder::new()
        .decode(&tile, &[quant], EntropyAlgorithm::Rlgr3)
        .unwrap_err();

    assert!(
        matches!(error, RfxDecodeError::InvalidQuantIndex { index: 2, table_len: 1 }),
        "{error:?}"
    );
}

#[test]
fn a_message_without_headers_decodes_every_tile_and_its_rectangles() {
    let quant = Quant::default();
    let zero = flat_component(0, &quant);
    let tile = |x, y| Tile {
        y_quant_index: 0,
        cb_quant_index: 0,
        cr_quant_index: 0,
        x,
        y,
        y_data: &zero,
        cb_data: &zero,
        cr_data: &zero,
    };
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
    let stream = encode_blocks(&[
        Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: 7,
            number_of_regions: 1,
        })),
        Block::CodecChannel(CodecChannel::Region(RegionPdu {
            rectangles: rectangles.clone(),
        })),
        Block::CodecChannel(CodecChannel::TileSet(TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
            quants: vec![quant],
            tiles: vec![tile(0, 0), tile(1, 1)],
        })),
        Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu)),
    ]);

    let message = MessageDecoder::new().decode(&stream).unwrap();

    assert_eq!(message.rectangles, rectangles);
    let positions: Vec<_> = message.tiles.iter().map(|tile| (tile.x_idx, tile.y_idx)).collect();
    assert_eq!(positions, [(0, 0), (1, 1)]);
    assert!(message.tiles.iter().all(|tile| tile.pixels.len() == TILE_RGBA_LEN));
}

#[test]
fn a_message_without_a_tileset_is_rejected() {
    let stream = encode_blocks(&[
        Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: 0,
            number_of_regions: 1,
        })),
        Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu)),
    ]);

    let error = MessageDecoder::new().decode(&stream).unwrap_err();

    assert!(matches!(error, RfxDecodeError::MissingBlock("TILESET")), "{error:?}");
}

#[test]
fn a_message_without_a_frame_begin_is_rejected() {
    let stream = encode_blocks(&[Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu))]);

    let error = MessageDecoder::new().decode(&stream).unwrap_err();

    assert!(
        matches!(error, RfxDecodeError::MissingBlock("FRAME_BEGIN")),
        "{error:?}"
    );
}
