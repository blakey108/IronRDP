use ironrdp_core::{Decode as _, Encode, ReadCursor, WriteCursor, encode_vec};
use ironrdp_dvc::DvcProcessor as _;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::decode::{DecodedFrame, DecoderResult, H264Decoder};
use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitiesConfirmPdu, CapabilitiesV8Flags, CapabilitySet, CapabilityVersion,
    Codec1Type, Color, CreateSurfacePdu, DeleteSurfacePdu, EndFramePdu, GfxPdu, MapSurfaceToOutputPdu,
    MapSurfaceToScaledWindowPdu, MapSurfaceToWindowPdu, PixelFormat, ResetGraphicsPdu, SolidFillPdu, StartFramePdu,
    Timestamp, WireToSurface1Pdu,
};
use ironrdp_graphics::clearcodec::ClearCodecEncoder;
use ironrdp_graphics::zgfx::wrap_uncompressed;
use ironrdp_pdu::geometry::ExclusiveRectangle;
use proptest::prelude::*;
use rstest::rstest;

// ============================================================================
// Test Handler
// ============================================================================

struct TestHandler {
    caps_confirmed: bool,
    bitmaps_received: Vec<(u16, Codec1Type)>,
    frames_completed: Vec<u32>,
    reset_count: u32,
}

impl TestHandler {
    fn new() -> Self {
        Self {
            caps_confirmed: false,
            bitmaps_received: Vec::new(),
            frames_completed: Vec::new(),
            reset_count: 0,
        }
    }
}

impl GraphicsPipelineHandler for TestHandler {
    fn on_capabilities_confirmed(&mut self, _caps: &CapabilitySet) {
        self.caps_confirmed = true;
    }

    fn on_reset_graphics(&mut self, _width: u32, _height: u32) {
        self.reset_count += 1;
    }

    fn on_surface_created(&mut self, _surface: &Surface) {}
    fn on_surface_deleted(&mut self, _surface_id: u16) {}
    fn on_surface_mapped(&mut self, _surface_id: u16, _x: u32, _y: u32) {}

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        self.bitmaps_received.push((update.surface_id, update.codec_id));
    }

    fn on_frame_complete(&mut self, frame_id: u32) {
        self.frames_completed.push(frame_id);
    }

    fn on_close(&mut self) {}
    fn on_unhandled_pdu(&mut self, _pdu: &GfxPdu) {}
}

// ============================================================================
// Mock H.264 Decoder
// ============================================================================

struct MockH264Decoder;

impl H264Decoder for MockH264Decoder {
    fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
        // Return a 16x16 solid red frame (macroblock-aligned minimum)
        let mut data = vec![0u8; 16 * 16 * 4];
        for pixel in data.chunks_exact_mut(4) {
            pixel[0] = 255; // R
            pixel[3] = 255; // A
        }
        Ok(DecodedFrame::new(data, 16, 16))
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn encode_pdu<T: Encode>(pdu: &T) -> Vec<u8> {
    let mut buf = vec![0u8; pdu.size()];
    let mut cursor = WriteCursor::new(&mut buf);
    pdu.encode(&mut cursor).expect("encode failed");
    buf
}

/// Encode a GfxPdu and wrap in a ZGFX uncompressed segment descriptor.
/// The client's process() expects ZGFX-segmented input (it runs decompression first).
fn encode_for_process(pdu: &GfxPdu) -> Vec<u8> {
    let raw = encode_pdu(pdu);
    wrap_uncompressed(&raw)
}

fn decode_caps_from_message(msg: &ironrdp_dvc::DvcMessage) -> CapabilitiesAdvertisePdu {
    let encoded = encode_vec(msg.as_ref()).expect("encode should succeed");
    let mut cursor = ReadCursor::new(&encoded);
    let pdu = GfxPdu::decode(&mut cursor).expect("decode should succeed");
    match pdu {
        GfxPdu::CapabilitiesAdvertise(caps) => caps,
        other => panic!("expected CapabilitiesAdvertise, got {other:?}"),
    }
}

/// Create a client, send CapabilitiesConfirm V8 through process(), and create a surface.
fn setup_active_client_with_surface(
    decoder: Option<Box<dyn H264Decoder>>,
    surface_id: u16,
    width: u16,
    height: u16,
) -> GraphicsPipelineClient {
    let handler = TestHandler::new();
    let mut client = GraphicsPipelineClient::new(Box::new(handler), decoder);

    // Activate via CapabilitiesConfirm
    let confirm = GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu::from_typed(&CapabilitySet::V8 {
        flags: CapabilitiesV8Flags::empty(),
    }));
    client
        .process(0, &encode_for_process(&confirm))
        .expect("confirm should succeed");

    // Create surface
    let create = GfxPdu::CreateSurface(CreateSurfacePdu {
        surface_id,
        width,
        height,
        pixel_format: PixelFormat::XRgb,
    });
    client
        .process(0, &encode_for_process(&create))
        .expect("create surface should succeed");

    client
}

// ============================================================================
// Tests: Capability Advertisement
// ============================================================================

#[test]
fn client_sends_capabilities_on_start() {
    let handler = TestHandler::new();
    let mut client = GraphicsPipelineClient::new(Box::new(handler), None);
    let messages = client.start(0).expect("start should succeed");
    assert_eq!(messages.len(), 1);
}

#[test]
fn client_filters_avc_caps_without_decoder() {
    let handler = TestHandler::new();
    let mut client = GraphicsPipelineClient::new(Box::new(handler), None);
    let messages = client.start(0).expect("start should succeed");
    assert_eq!(messages.len(), 1);

    let caps_pdu = decode_caps_from_message(&messages[0]);
    assert_eq!(
        caps_pdu.0.len(),
        1,
        "expected exactly one capability set when no decoder is present"
    );
    assert!(
        caps_pdu.0[0].version == CapabilityVersion::V8,
        "expected only V8 capability set without decoder, got {:?}",
        caps_pdu.0[0]
    );
}

#[test]
fn client_keeps_avc_caps_with_decoder() {
    let handler = TestHandler::new();
    let mut client = GraphicsPipelineClient::new(Box::new(handler), Some(Box::new(MockH264Decoder)));
    let messages = client.start(0).expect("start should succeed");
    assert_eq!(messages.len(), 1);

    let caps_pdu = decode_caps_from_message(&messages[0]);
    assert_eq!(
        caps_pdu.0.len(),
        2,
        "expected both capability sets with decoder present"
    );
    // V10.x is absent by design: those versions imply AVC444, which the client
    // cannot decode, and the server would then send only frames it discards.
    assert_eq!(caps_pdu.0[0].version, CapabilityVersion::V8_1);
    assert_eq!(caps_pdu.0[1].version, CapabilityVersion::V8);
}

// ============================================================================
// Tests: Frame Flow (via process() with encoded PDUs)
// ============================================================================

#[test]
fn client_sends_frame_ack_on_end_frame() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    let end = GfxPdu::EndFrame(EndFramePdu { frame_id: 42 });
    let responses = client
        .process(0, &encode_for_process(&end))
        .expect("end frame should succeed");

    assert_eq!(responses.len(), 1, "should produce exactly one FrameAcknowledge");
    assert_eq!(client.total_frames_decoded(), 1);
}

#[test]
fn client_handles_uncompressed_via_process() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Uncompressed,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: vec![0u8; 4 * 4 * 4],
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("uncompressed should succeed");
}

#[test]
fn client_dispatches_avc420_via_process() {
    let mut client = setup_active_client_with_surface(Some(Box::new(MockH264Decoder)), 1, 16, 16);

    // Build minimal AVC420 bitmap stream
    let mut bitmap_data = Vec::new();
    bitmap_data.extend_from_slice(&1u32.to_le_bytes()); // nRect = 1
    bitmap_data.extend_from_slice(&0u16.to_le_bytes()); // left
    bitmap_data.extend_from_slice(&0u16.to_le_bytes()); // top
    bitmap_data.extend_from_slice(&15u16.to_le_bytes()); // right
    bitmap_data.extend_from_slice(&15u16.to_le_bytes()); // bottom
    bitmap_data.push(22); // qp
    bitmap_data.push(100); // quality
    bitmap_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]); // fake H.264

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Avc420,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        },
        bitmap_data,
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("AVC420 should succeed");
}

#[test]
fn client_skips_avc420_without_decoder() {
    let mut client = setup_active_client_with_surface(None, 1, 16, 16);

    let mut bitmap_data = Vec::new();
    bitmap_data.extend_from_slice(&1u32.to_le_bytes());
    bitmap_data.extend_from_slice(&0u16.to_le_bytes());
    bitmap_data.extend_from_slice(&0u16.to_le_bytes());
    bitmap_data.extend_from_slice(&15u16.to_le_bytes());
    bitmap_data.extend_from_slice(&15u16.to_le_bytes());
    bitmap_data.push(22);
    bitmap_data.push(100);
    bitmap_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Avc420,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        },
        bitmap_data,
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("should succeed without decoder");
}

#[test]
fn client_frame_ordering_via_process() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    // StartFrame
    let start = GfxPdu::StartFrame(StartFramePdu {
        timestamp: Timestamp {
            milliseconds: 0,
            seconds: 0,
            minutes: 0,
            hours: 0,
        },
        frame_id: 1,
    });
    client.process(0, &encode_for_process(&start)).expect("start frame");

    // WireToSurface1 (uncompressed)
    let wire = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Uncompressed,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: vec![0u8; 4 * 4 * 4],
    });
    client.process(0, &encode_for_process(&wire)).expect("wire to surface");

    // EndFrame should produce FrameAcknowledge
    let end = GfxPdu::EndFrame(EndFramePdu { frame_id: 1 });
    let responses = client.process(0, &encode_for_process(&end)).expect("end frame");

    assert_eq!(responses.len(), 1);
    assert_eq!(client.total_frames_decoded(), 1);
}

// ============================================================================
// Tests: Surface Lifecycle (via process())
// ============================================================================

#[test]
fn client_creates_and_queries_surface() {
    let client = setup_active_client_with_surface(None, 7, 1920, 1080);

    let surface = client.get_surface(7);
    assert!(surface.is_some(), "surface 7 should exist after creation");
    assert_eq!(surface.unwrap().width, 1920);
    assert_eq!(surface.unwrap().height, 1080);

    // Nonexistent surface
    assert!(client.get_surface(99).is_none());
}

#[test]
fn client_deletes_surface_via_process() {
    let mut client = setup_active_client_with_surface(None, 5, 100, 100);
    assert!(client.get_surface(5).is_some());

    let delete = GfxPdu::DeleteSurface(DeleteSurfacePdu { surface_id: 5 });
    client
        .process(0, &encode_for_process(&delete))
        .expect("delete should succeed");

    assert!(client.get_surface(5).is_none(), "surface should be gone after delete");
}

#[test]
fn client_resets_surfaces_via_process() {
    let mut client = setup_active_client_with_surface(None, 1, 100, 100);

    // Create a second surface
    let create2 = GfxPdu::CreateSurface(CreateSurfacePdu {
        surface_id: 2,
        width: 200,
        height: 200,
        pixel_format: PixelFormat::XRgb,
    });
    client.process(0, &encode_for_process(&create2)).expect("create 2");
    assert!(client.get_surface(1).is_some());
    assert!(client.get_surface(2).is_some());

    // ResetGraphics should clear all surfaces
    let reset = GfxPdu::ResetGraphics(ResetGraphicsPdu {
        width: 1920,
        height: 1080,
        monitors: vec![],
    });
    client.process(0, &encode_for_process(&reset)).expect("reset");

    assert!(
        client.get_surface(1).is_none(),
        "surface 1 should be cleared after reset"
    );
    assert!(
        client.get_surface(2).is_none(),
        "surface 2 should be cleared after reset"
    );
}

// ============================================================================
// Tests: Error Handling
// ============================================================================

#[test]
fn client_rejects_wire_to_unknown_surface() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 99, // does not exist
        codec_id: Codec1Type::Uncompressed,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: vec![0u8; 4 * 4 * 4],
    });
    let result = client.process(0, &encode_for_process(&pdu));
    assert!(result.is_err(), "should reject write to nonexistent surface");
}

#[test]
fn client_rejects_invalid_rectangle_ordering() {
    let mut client = setup_active_client_with_surface(None, 1, 100, 100);

    // left > right
    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Uncompressed,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 50,
            top: 0,
            right: 10,
            bottom: 10,
        },
        bitmap_data: vec![0u8; 4],
    });
    let result = client.process(0, &encode_for_process(&pdu));
    assert!(result.is_err(), "left > right should be rejected");
}

#[test]
fn client_tolerates_out_of_bounds_rectangle() {
    let mut client = setup_active_client_with_surface(None, 1, 100, 100);

    // Rectangle exceeds surface dimensions. The client logs a warning
    // but continues processing (defensive: avoid disconnecting for a
    // recoverable server-side error).
    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::Uncompressed,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 200, // exceeds surface width of 100
            bottom: 50,
        },
        bitmap_data: vec![0u8; 200 * 50 * 4],
    });
    let result = client.process(0, &encode_for_process(&pdu));
    assert!(
        result.is_ok(),
        "out-of-bounds rectangle should be tolerated (warn, not error)"
    );
}

// ============================================================================
// Tests: ClearCodec Decode
// ============================================================================

#[test]
fn client_dispatches_clearcodec_via_process() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    // Encode a valid ClearCodec frame: 4x4 solid red (BGRA: B=0, G=0, R=255, A=255)
    let mut cc_enc = ClearCodecEncoder::new();
    let bgra: Vec<u8> = (0..16).flat_map(|_| [0x00u8, 0x00, 0xFF, 0xFF]).collect();
    let cc_data = cc_enc.encode(&bgra, 4, 4);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::ClearCodec,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: cc_data,
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("ClearCodec decode should succeed");
}

#[test]
fn client_clearcodec_produces_rgba_output() {
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CapturedBitmap {
        data: Option<Vec<u8>>,
        codec: Option<Codec1Type>,
    }

    struct CapturingHandler {
        captured: Arc<Mutex<CapturedBitmap>>,
    }

    impl GraphicsPipelineHandler for CapturingHandler {
        fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
            let mut cap = self.captured.lock().unwrap();
            cap.data = Some(update.data.clone());
            cap.codec = Some(update.codec_id);
        }
    }

    let captured = Arc::new(Mutex::new(CapturedBitmap::default()));
    let handler = CapturingHandler {
        captured: Arc::clone(&captured),
    };
    let mut client = GraphicsPipelineClient::new(Box::new(handler), None);

    // Activate and create surface
    let confirm = GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu::from_typed(&CapabilitySet::V8 {
        flags: CapabilitiesV8Flags::empty(),
    }));
    client.process(0, &encode_for_process(&confirm)).expect("confirm");

    let create = GfxPdu::CreateSurface(CreateSurfacePdu {
        surface_id: 1,
        width: 2,
        height: 1,
        pixel_format: PixelFormat::XRgb,
    });
    client.process(0, &encode_for_process(&create)).expect("create surface");

    // Encode a 2x1 frame: pixel 0 = blue (BGRA: FF,00,00,FF), pixel 1 = green (BGRA: 00,FF,00,FF)
    let mut cc_enc = ClearCodecEncoder::new();
    let bgra = vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF];
    let cc_data = cc_enc.encode(&bgra, 2, 1);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::ClearCodec,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 2,
            bottom: 1,
        },
        bitmap_data: cc_data,
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("ClearCodec should succeed");

    // Verify BGRA-to-RGBA conversion: blue (BGRA FF,00,00,FF) -> RGBA (00,00,FF,FF)
    let cap = captured.lock().unwrap();
    assert_eq!(cap.codec, Some(Codec1Type::ClearCodec));
    let bitmap = cap.data.as_ref().expect("handler should have received bitmap");
    assert_eq!(bitmap.len(), 8, "2 pixels * 4 bytes");
    assert_eq!(&bitmap[0..4], &[0x00, 0x00, 0xFF, 0xFF], "pixel 0: blue in RGBA");
    assert_eq!(&bitmap[4..8], &[0x00, 0xFF, 0x00, 0xFF], "pixel 1: green in RGBA");
}

/// A `ResetGraphics` must not drop the ClearCodec glyph cache. Per MS-RDPEGFX 3.3.5.14 it only
/// resizes the Graphics Output Buffer; cache lifetime is driven by the stream through
/// CLEARCODEC_FLAG_CACHE_RESET (2.2.4.1). A server may therefore send a GLYPH_HIT after a reset
/// for a glyph it cached before it, and the client has to still have it.
///
/// The encoder is reused across the reset on purpose: `ClearCodecEncoder::encode` emits a glyph
/// hit when it finds the same pixels in its own cache, so re-encoding the identical tile is what
/// produces the post-reset GLYPH_HIT. A fresh encoder would start with an empty cache and send a
/// full frame instead, which exercises nothing.
#[test]
fn client_clearcodec_glyph_cache_survives_reset() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    // Encode and decode a ClearCodec frame
    let mut cc_enc = ClearCodecEncoder::new();
    let bgra: Vec<u8> = (0..16).flat_map(|_| [0x00u8, 0x00, 0xFF, 0xFF]).collect();
    let cc_data = cc_enc.encode(&bgra, 4, 4);

    let pdu = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 1,
        codec_id: Codec1Type::ClearCodec,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: cc_data,
    });
    client
        .process(0, &encode_for_process(&pdu))
        .expect("pre-reset ClearCodec should succeed");

    // Reset graphics. Surfaces are cleared; the ClearCodec caches are not.
    let reset = GfxPdu::ResetGraphics(ResetGraphicsPdu {
        width: 1920,
        height: 1080,
        monitors: vec![],
    });
    client
        .process(0, &encode_for_process(&reset))
        .expect("reset should succeed");

    // Re-create surface and decode another frame
    let create = GfxPdu::CreateSurface(CreateSurfacePdu {
        surface_id: 2,
        width: 4,
        height: 4,
        pixel_format: PixelFormat::XRgb,
    });
    client
        .process(0, &encode_for_process(&create))
        .expect("create surface after reset");

    // Same encoder, same pixels: this hits its glyph cache and emits a GLYPH_HIT referencing the
    // index the decoder stored before the reset.
    let cc_data2 = cc_enc.encode(&bgra, 4, 4);

    let pdu2 = GfxPdu::WireToSurface1(WireToSurface1Pdu {
        surface_id: 2,
        codec_id: Codec1Type::ClearCodec,
        pixel_format: PixelFormat::XRgb,
        destination_rectangle: ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        },
        bitmap_data: cc_data2,
    });
    client
        .process(0, &encode_for_process(&pdu2))
        .expect("post-reset GLYPH_HIT should resolve against the surviving glyph cache");
}

// ============================================================================
// Tests: Multiple Frames
// ============================================================================

#[test]
fn client_tracks_frame_count_across_multiple_frames() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);

    for frame_id in 1..=5 {
        let end = GfxPdu::EndFrame(EndFramePdu { frame_id });
        client.process(0, &encode_for_process(&end)).expect("end frame");
    }

    assert_eq!(client.total_frames_decoded(), 5);
}

// ============================================================================
// Tests: Close
// ============================================================================

#[test]
fn client_close_transitions_to_inactive() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);
    assert!(client.is_active());

    client.close(0);
    assert!(!client.is_active(), "client should not be active after close");
}

// ============================================================================
// Tests: RemoteApp window mapping (MS-RDPEGFX 2.2.2.20, 2.2.2.23)
// ============================================================================

const FILL_COLOR: Color = Color {
    b: 0x33,
    g: 0x22,
    r: 0x11,
    xa: 0,
};
const FILL_RGBA: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];

fn fill_frame(client: &mut GraphicsPipelineClient, surface_id: u16, rectangle: ExclusiveRectangle) {
    for pdu in [
        GfxPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id: 1,
        }),
        GfxPdu::SolidFill(SolidFillPdu {
            surface_id,
            fill_pixel: FILL_COLOR,
            rectangles: vec![rectangle],
        }),
        GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
    ] {
        client.process(0, &encode_for_process(&pdu)).expect("frame PDU");
    }
}

/// Applies a mapping PDU, commits the frame and discards what the mapping itself exposed.
fn map_and_settle(client: &mut GraphicsPipelineClient, map: GfxPdu) {
    client.process(0, &encode_for_process(&map)).expect("map surface");
    client
        .process(0, &encode_for_process(&GfxPdu::EndFrame(EndFramePdu { frame_id: 0 })))
        .expect("end frame");
    let _ = client.drain_window_updates();
    let _ = client.drain_output();
}

/// A client with an `output_width` by `output_height` graphics output and one surface.
///
/// `ResetGraphics` deletes every surface, so the surface is created after it.
fn client_with_output(
    (output_width, output_height): (u32, u32),
    surface_id: u16,
    (width, height): (u16, u16),
) -> GraphicsPipelineClient {
    let mut client = setup_active_client_with_surface(None, surface_id, width, height);
    for pdu in [
        GfxPdu::ResetGraphics(ResetGraphicsPdu {
            width: output_width,
            height: output_height,
            monitors: vec![],
        }),
        GfxPdu::CreateSurface(CreateSurfacePdu {
            surface_id,
            width,
            height,
            pixel_format: PixelFormat::XRgb,
        }),
    ] {
        client.process(0, &encode_for_process(&pdu)).expect("output setup");
    }
    client
}

fn map_to_output(surface_id: u16) -> GfxPdu {
    GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
        surface_id,
        output_origin_x: 0,
        output_origin_y: 0,
    })
}

fn map_to_window(surface_id: u16, window_id: u64, (mapped_width, mapped_height): (u32, u32)) -> GfxPdu {
    GfxPdu::MapSurfaceToWindow(MapSurfaceToWindowPdu {
        surface_id,
        window_id,
        mapped_width,
        mapped_height,
    })
}

// oracle: spec MS-RDPEGFX §3.3.5.20 (the surface's top-left mappedWidth x mappedHeight area is presented in the window)
#[test]
fn window_mapped_surface_paints_its_window_instead_of_the_output() {
    let mut client = setup_active_client_with_surface(None, 1, 8, 4);
    map_and_settle(&mut client, map_to_window(1, 0x42, (4, 2)));

    fill_frame(&mut client, 1, rect(0, 0, 8, 4));

    assert!(client.drain_output().is_empty());
    let updates = client.drain_window_updates();
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(update.window_id, 0x42);
    assert_eq!((update.width, update.height), (4, 2));
    // Only the mapped top-left 4x2 area reaches the window.
    assert_eq!(update.region, rect(0, 0, 4, 2));
    assert_eq!(update.data, FILL_RGBA.repeat(8));
    assert!(client.drain_window_updates().is_empty(), "draining empties the queue");
}

// oracle: mutant output mapping left in force after a window remap
#[test]
fn remapping_an_output_surface_to_a_window_stops_painting_the_output() {
    let mut client = client_with_output((8, 4), 1, (8, 4));
    map_and_settle(&mut client, map_to_output(1));
    map_and_settle(&mut client, map_to_window(1, 7, (8, 4)));

    fill_frame(&mut client, 1, rect(0, 0, 8, 4));

    assert!(client.drain_output().is_empty());
    assert_eq!(client.drain_window_updates().len(), 1);
}

// oracle: mutant window mapping left in force after an output remap
#[test]
fn remapping_a_window_surface_to_the_output_stops_painting_the_window() {
    let mut client = client_with_output((8, 4), 1, (8, 4));
    map_and_settle(&mut client, map_to_window(1, 7, (8, 4)));
    map_and_settle(&mut client, map_to_output(1));

    fill_frame(&mut client, 1, rect(0, 0, 8, 4));

    assert!(client.drain_window_updates().is_empty());
    assert_eq!(client.drain_output().len(), 1);
}

// oracle: spec MS-RDPEGFX §3.3.5.23 (the mapped area is presented at targetWidth x targetHeight)
#[test]
fn scaled_window_mapping_scales_dirty_regions_to_the_target_size() {
    let mut client = setup_active_client_with_surface(None, 1, 2, 2);
    map_and_settle(
        &mut client,
        GfxPdu::MapSurfaceToScaledWindow(MapSurfaceToScaledWindowPdu {
            surface_id: 1,
            window_id: 7,
            mapped_width: 2,
            mapped_height: 2,
            target_width: 4,
            target_height: 4,
        }),
    );

    fill_frame(&mut client, 1, rect(1, 1, 2, 2));

    let updates = client.drain_window_updates();
    assert_eq!(updates.len(), 1);
    assert_eq!((updates[0].width, updates[0].height), (4, 4));
    // oracle: mutant nearest-neighbor edge mapping (the spec does not fix the sampler)
    // Source pixel (1, 1) covers target pixels (2..4, 2..4) at a 2x scale.
    assert_eq!(updates[0].region, rect(2, 2, 4, 4));
    assert_eq!(updates[0].data, FILL_RGBA.repeat(4));
}

// oracle: reference FreeRDP/FreeRDP@16a7c2ec2fd0:libfreerdp/gdi/gfx.c (gdi_MapSurfaceToWindow refuses a mapped area larger than the surface)
#[test]
fn window_mapping_larger_than_the_surface_is_ignored() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);
    map_and_settle(&mut client, map_to_window(1, 7, (8, 8)));

    fill_frame(&mut client, 1, rect(0, 0, 4, 4));

    assert!(client.drain_window_updates().is_empty());
    assert!(client.drain_output().is_empty());
}

// oracle: reference FreeRDP/FreeRDP@16a7c2ec2fd0:libfreerdp/gdi/gfx.c (gdi_MapSurfaceToWindow refuses an oversized mapping without touching the surface's existing mapping)
#[test]
fn rejected_window_mapping_keeps_the_previous_window() {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);
    map_and_settle(&mut client, map_to_window(1, 7, (4, 4)));
    map_and_settle(&mut client, map_to_window(1, 8, (8, 8)));

    fill_frame(&mut client, 1, rect(0, 0, 4, 4));

    let updates = client.drain_window_updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].window_id, 7);
}

// oracle: reference FreeRDP/FreeRDP@16a7c2ec2fd0:libfreerdp/gdi/gfx.c (gdi_MapSurfaceToWindow refuses an oversized mapping without touching the surface's existing mapping)
#[test]
fn rejected_window_mapping_keeps_the_output_mapping() {
    let mut client = client_with_output((4, 4), 1, (4, 4));
    map_and_settle(&mut client, map_to_output(1));
    map_and_settle(&mut client, map_to_window(1, 8, (8, 8)));

    fill_frame(&mut client, 1, rect(0, 0, 4, 4));

    assert!(client.drain_window_updates().is_empty());
    assert_eq!(client.drain_output().len(), 1);
}

// oracle: mutant empty or oversized target sizes reach the scaling arithmetic
#[rstest]
#[case::empty_target((0, 4))]
#[case::target_past_max_output_dim((32_767, 4))]
#[case::target_past_u16((70_000, 4))]
fn window_mapping_with_an_unpresentable_target_is_ignored(#[case] (target_width, target_height): (u32, u32)) {
    let mut client = setup_active_client_with_surface(None, 1, 4, 4);
    map_and_settle(
        &mut client,
        GfxPdu::MapSurfaceToScaledWindow(MapSurfaceToScaledWindowPdu {
            surface_id: 1,
            window_id: 7,
            mapped_width: 4,
            mapped_height: 4,
            target_width,
            target_height,
        }),
    );

    fill_frame(&mut client, 1, rect(0, 0, 4, 4));

    assert!(client.drain_window_updates().is_empty());
}

// oracle: spec MS-RDPEGFX §3.3.5.23 (window content is targetWidth x targetHeight, so every
// region lies inside it; window-mapped surfaces never reach the graphics output)
proptest! {
    #[test]
    fn window_updates_stay_inside_the_target_and_off_the_output(
        (surface_width, surface_height, mapped_width, mapped_height) in (1u16..=16, 1u16..=16)
            .prop_flat_map(|(width, height)| (Just(width), Just(height), 1u16..=width, 1u16..=height)),
        target_width in 1u16..=40,
        target_height in 1u16..=40,
        fill in (0u16..20, 0u16..20, 0u16..20, 0u16..20),
    ) {
        let mut client = client_with_output((64, 64), 1, (surface_width, surface_height));
        map_and_settle(
            &mut client,
            GfxPdu::MapSurfaceToScaledWindow(MapSurfaceToScaledWindowPdu {
                surface_id: 1,
                window_id: 7,
                mapped_width: u32::from(mapped_width),
                mapped_height: u32::from(mapped_height),
                target_width: u32::from(target_width),
                target_height: u32::from(target_height),
            }),
        );

        let (left, top) = (fill.0.min(fill.2), fill.1.min(fill.3));
        let (right, bottom) = (fill.0.max(fill.2), fill.1.max(fill.3));
        fill_frame(&mut client, 1, rect(left, top, right, bottom));

        prop_assert!(client.drain_output().is_empty());
        for update in client.drain_window_updates() {
            prop_assert_eq!((update.width, update.height), (target_width, target_height));
            let region = update.region;
            prop_assert!(region.left < region.right && region.top < region.bottom);
            prop_assert!(region.right <= target_width && region.bottom <= target_height);
            let area = usize::from(region.right - region.left) * usize::from(region.bottom - region.top);
            prop_assert_eq!(update.data.len(), area * 4);
        }
        prop_assert!(client.drain_window_updates().is_empty());
    }
}

fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
    ExclusiveRectangle {
        left,
        top,
        right,
        bottom,
    }
}
