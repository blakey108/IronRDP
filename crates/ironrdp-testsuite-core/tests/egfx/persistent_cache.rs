use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ironrdp_core::{Decode as _, ReadCursor, encode_vec};
use ironrdp_dvc::{DvcMessage, DvcProcessor as _};
use ironrdp_egfx::cache::{
    CachedBitmap, MAX_CACHE_IMPORT_ENTRIES, PersistentCacheEntry, PersistentCacheError, PersistentCacheStore,
    SMALL_CACHE_BYTES, SMALL_CACHE_SLOTS,
};
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::pdu::{
    CacheEntryMetadata, CacheImportOfferPdu, CacheImportReplyPdu, CacheToSurfacePdu, CapabilitiesAdvertisePdu,
    CapabilitiesConfirmPdu, CapabilitiesV8Flags, CapabilitiesV81Flags, CapabilitySet, CreateSurfacePdu, EndFramePdu,
    EvictCacheEntryPdu, GfxPdu, MapSurfaceToOutputPdu, PixelFormat, Point, ResetGraphicsPdu, SolidFillPdu,
    StartFramePdu, SurfaceToCachePdu, Timestamp,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler as ServerHandler, GraphicsPipelineServer, QoeMetrics, Surface};
use ironrdp_graphics::zgfx::{Decompressor, wrap_uncompressed};
use ironrdp_pdu::geometry::ExclusiveRectangle;

// ============================================================================
// In-memory store
// ============================================================================

#[derive(Default)]
struct StoreState {
    entries: Vec<PersistentCacheEntry>,
    loads: Vec<u64>,
    import_finished: u32,
    saved: Option<Vec<PersistentCacheEntry>>,
}

#[derive(Clone, Default)]
struct MemoryStore(Arc<Mutex<StoreState>>);

impl MemoryStore {
    fn with_entries(entries: Vec<PersistentCacheEntry>) -> Self {
        let store = Self::default();
        store.0.lock().unwrap().entries = entries;
        store
    }

    fn state(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.0.lock().unwrap()
    }
}

impl PersistentCacheStore for MemoryStore {
    fn entries(&mut self) -> Result<Vec<CacheEntryMetadata>, PersistentCacheError> {
        Ok(self
            .state()
            .entries
            .iter()
            .map(|entry| CacheEntryMetadata {
                cache_key: entry.cache_key,
                bitmap_len: u32::try_from(entry.data.len()).unwrap(),
            })
            .collect())
    }

    fn load(&mut self, cache_key: u64) -> Result<Option<PersistentCacheEntry>, PersistentCacheError> {
        let mut state = self.state();
        state.loads.push(cache_key);
        Ok(state.entries.iter().find(|entry| entry.cache_key == cache_key).cloned())
    }

    fn import_finished(&mut self) {
        self.state().import_finished += 1;
    }

    fn save(&mut self, entries: &[CachedBitmap<'_>]) -> Result<(), PersistentCacheError> {
        self.state().saved = Some(
            entries
                .iter()
                .map(|entry| PersistentCacheEntry {
                    cache_key: entry.cache_key,
                    width: entry.width,
                    height: entry.height,
                    data: entry.data.to_vec(),
                })
                .collect(),
        );
        Ok(())
    }
}

/// A store that only lists metadata, for offer-limit tests that never load.
struct MetadataStore(Vec<CacheEntryMetadata>);

impl PersistentCacheStore for MetadataStore {
    fn entries(&mut self) -> Result<Vec<CacheEntryMetadata>, PersistentCacheError> {
        Ok(self.0.clone())
    }

    fn load(&mut self, _cache_key: u64) -> Result<Option<PersistentCacheEntry>, PersistentCacheError> {
        Ok(None)
    }

    fn save(&mut self, _entries: &[CachedBitmap<'_>]) -> Result<(), PersistentCacheError> {
        Ok(())
    }
}

struct NoopHandler;

impl GraphicsPipelineHandler for NoopHandler {}

// ============================================================================
// Helpers
// ============================================================================

fn solid_entry(cache_key: u64, width: u16, height: u16, rgba: [u8; 4]) -> PersistentCacheEntry {
    PersistentCacheEntry {
        cache_key,
        width,
        height,
        data: rgba.repeat(usize::from(width) * usize::from(height)),
    }
}

fn send(client: &mut GraphicsPipelineClient, pdu: GfxPdu) -> Vec<GfxPdu> {
    let payload = wrap_uncompressed(&encode_vec(&pdu).unwrap());
    client
        .process(0, &payload)
        .expect("client processes the PDU")
        .iter()
        .map(decode_message)
        .collect()
}

fn decode_message(message: &DvcMessage) -> GfxPdu {
    let encoded = encode_vec(message.as_ref()).unwrap();
    GfxPdu::decode(&mut ReadCursor::new(&encoded)).unwrap()
}

fn confirm(client: &mut GraphicsPipelineClient, caps: CapabilitySet) -> Vec<GfxPdu> {
    send(
        client,
        GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu::from_typed(&caps)),
    )
}

fn full_cache_caps() -> CapabilitySet {
    CapabilitySet::V8 {
        flags: CapabilitiesV8Flags::empty(),
    }
}

fn offered_keys(responses: &[GfxPdu]) -> Vec<u64> {
    match responses {
        [GfxPdu::CacheImportOffer(offer)] => offer.cache_entries.iter().map(|entry| entry.cache_key).collect(),
        other => panic!("expected one CacheImportOffer, got {other:?}"),
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

const TIMESTAMP: Timestamp = Timestamp {
    milliseconds: 0,
    seconds: 0,
    minutes: 0,
    hours: 0,
};

/// Reset the output to `width` x `height` and map surface 1 of that size at the origin.
fn map_surface(client: &mut GraphicsPipelineClient, width: u16, height: u16) {
    send(
        client,
        GfxPdu::ResetGraphics(ResetGraphicsPdu {
            width: u32::from(width),
            height: u32::from(height),
            monitors: vec![],
        }),
    );
    send(
        client,
        GfxPdu::CreateSurface(CreateSurfacePdu {
            surface_id: 1,
            width,
            height,
            pixel_format: PixelFormat::XRgb,
        }),
    );
    send(
        client,
        GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
            surface_id: 1,
            output_origin_x: 0,
            output_origin_y: 0,
        }),
    );
}

/// Paste `cache_slot` at the surface origin inside one frame and return the output pixel at (0, 0).
fn paste_and_read_origin(client: &mut GraphicsPipelineClient, cache_slot: u16) -> Option<[u8; 4]> {
    let _ = client.drain_output();
    send(
        client,
        GfxPdu::StartFrame(StartFramePdu {
            timestamp: TIMESTAMP,
            frame_id: 1,
        }),
    );
    send(
        client,
        GfxPdu::CacheToSurface(CacheToSurfacePdu {
            cache_slot,
            surface_id: 1,
            destination_points: vec![Point { x: 0, y: 0 }],
        }),
    );
    send(client, GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }));

    let updates = client.drain_output();
    let update = updates
        .iter()
        .find(|update| update.region.left == 0 && update.region.top == 0)?;
    Some(update.data[..4].try_into().unwrap())
}

// ============================================================================
// Offer
// ============================================================================

#[test]
fn no_offer_without_a_store() {
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None);
    assert!(confirm(&mut client, full_cache_caps()).is_empty());
}

#[test]
fn no_offer_from_an_empty_store() {
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MemoryStore::default()));
    assert!(confirm(&mut client, full_cache_caps()).is_empty());
}

#[test]
fn the_offer_follows_the_capabilities_confirm_in_store_order() {
    let store = MemoryStore::with_entries(vec![
        solid_entry(30, 8, 8, [1, 2, 3, 255]),
        solid_entry(10, 4, 4, [4, 5, 6, 255]),
        solid_entry(20, 2, 2, [7, 8, 9, 255]),
    ]);
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store));

    let responses = confirm(&mut client, full_cache_caps());

    let [GfxPdu::CacheImportOffer(offer)] = responses.as_slice() else {
        panic!("expected one CacheImportOffer, got {responses:?}");
    };
    assert_eq!(
        offer.cache_entries,
        [
            CacheEntryMetadata {
                cache_key: 30,
                bitmap_len: 8 * 8 * 4
            },
            CacheEntryMetadata {
                cache_key: 10,
                bitmap_len: 4 * 4 * 4
            },
            CacheEntryMetadata {
                cache_key: 20,
                bitmap_len: 2 * 2 * 4
            },
        ]
    );
}

#[test]
fn the_offer_never_reaches_the_protocol_entry_limit() {
    let entries = (0..u64::try_from(MAX_CACHE_IMPORT_ENTRIES).unwrap() + 100)
        .map(|cache_key| CacheEntryMetadata {
            cache_key,
            bitmap_len: 64,
        })
        .collect();
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MetadataStore(entries)));

    let keys = offered_keys(&confirm(&mut client, full_cache_caps()));

    // 2.2.2.16: cacheEntriesCount MUST be less than 5462.
    assert_eq!(keys.len(), MAX_CACHE_IMPORT_ENTRIES);
    assert!(keys.len() < 5462);
}

#[test]
fn small_cache_limits_the_offer_to_its_bytes() {
    const ENTRY_BYTES: u32 = 64 * 64 * 4;
    let entries = (0..2000)
        .map(|cache_key| CacheEntryMetadata {
            cache_key,
            bitmap_len: ENTRY_BYTES,
        })
        .collect::<Vec<_>>();

    let mut small = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MetadataStore(entries.clone())));
    let small_keys = offered_keys(&confirm(
        &mut small,
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::SMALL_CACHE,
        },
    ));
    assert_eq!(
        small_keys.len(),
        SMALL_CACHE_BYTES / usize::try_from(ENTRY_BYTES).unwrap()
    );

    let mut full = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MetadataStore(entries)));
    assert_eq!(offered_keys(&confirm(&mut full, full_cache_caps())).len(), 2000);
}

#[test]
fn thin_client_limits_the_offer_to_the_small_cache_slots() {
    let entries = (0..u64::from(SMALL_CACHE_SLOTS) + 500)
        .map(|cache_key| CacheEntryMetadata {
            cache_key,
            bitmap_len: 4,
        })
        .collect();
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MetadataStore(entries)));

    let keys = offered_keys(&confirm(
        &mut client,
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::THIN_CLIENT,
        },
    ));

    assert_eq!(keys.len(), usize::from(SMALL_CACHE_SLOTS));
}

#[test]
fn duplicate_and_empty_entries_are_not_offered() {
    let entries = vec![
        CacheEntryMetadata {
            cache_key: 1,
            bitmap_len: 16,
        },
        CacheEntryMetadata {
            cache_key: 1,
            bitmap_len: 16,
        },
        CacheEntryMetadata {
            cache_key: 2,
            bitmap_len: 0,
        },
        CacheEntryMetadata {
            cache_key: 3,
            bitmap_len: 16,
        },
    ];
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None)
        .with_persistent_cache(Box::new(MetadataStore(entries)));

    assert_eq!(offered_keys(&confirm(&mut client, full_cache_caps())), [1, 3]);
}

// ============================================================================
// Reply
// ============================================================================

#[test]
fn the_reply_preloads_each_accepted_entry_into_its_slot() {
    let store = MemoryStore::with_entries(vec![
        solid_entry(100, 4, 4, [255, 0, 0, 255]),
        solid_entry(200, 4, 4, [0, 255, 0, 255]),
        solid_entry(300, 4, 4, [0, 0, 255, 255]),
    ]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(&mut client, full_cache_caps());

    // The first two entries are accepted, into slots 9 and 4.
    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu {
            cache_slots: vec![9, 4],
        }),
    );

    assert_eq!(store.state().loads, [100, 200], "only accepted entries are read");
    assert_eq!(store.state().import_finished, 1);

    map_surface(&mut client, 16, 16);
    assert_eq!(paste_and_read_origin(&mut client, 9), Some([255, 0, 0, 255]));
    assert_eq!(paste_and_read_origin(&mut client, 4), Some([0, 255, 0, 255]));
}

#[test]
fn a_reply_without_an_offer_is_ignored() {
    let store = MemoryStore::with_entries(vec![solid_entry(100, 4, 4, [255, 0, 0, 255])]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));

    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu { cache_slots: vec![1] }),
    );

    assert!(store.state().loads.is_empty());
}

#[test]
fn a_second_reply_to_one_offer_is_ignored() {
    let store = MemoryStore::with_entries(vec![solid_entry(100, 4, 4, [255, 0, 0, 255])]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(&mut client, full_cache_caps());

    let reply = GfxPdu::CacheImportReply(CacheImportReplyPdu { cache_slots: vec![1] });
    send(&mut client, reply.clone());
    send(&mut client, reply);

    assert_eq!(store.state().loads, [100]);
}

#[test]
fn reply_slots_outside_the_confirmed_cache_are_skipped() {
    let store = MemoryStore::with_entries(vec![
        solid_entry(1, 2, 2, [10, 10, 10, 255]),
        solid_entry(2, 2, 2, [20, 20, 20, 255]),
        solid_entry(3, 2, 2, [30, 30, 30, 255]),
    ]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(
        &mut client,
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        },
    );

    // Slots are one-based, and the small cache has 4096 of them.
    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu {
            cache_slots: vec![0, SMALL_CACHE_SLOTS + 1, SMALL_CACHE_SLOTS],
        }),
    );

    assert_eq!(store.state().loads, [3]);
    map_surface(&mut client, 4, 4);
    assert_eq!(
        paste_and_read_origin(&mut client, SMALL_CACHE_SLOTS),
        Some([30, 30, 30, 255])
    );
}

#[test]
fn a_reply_naming_more_entries_than_offered_imports_only_the_offered_ones() {
    let store = MemoryStore::with_entries(vec![solid_entry(7, 2, 2, [70, 70, 70, 255])]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(&mut client, full_cache_caps());

    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu {
            cache_slots: vec![5, 6, 7],
        }),
    );

    assert_eq!(store.state().loads, [7]);
}

#[test]
fn an_entry_whose_pixels_do_not_match_its_size_is_not_imported() {
    let mut malformed = solid_entry(1, 4, 4, [1, 1, 1, 255]);
    malformed.data.truncate(10);
    let store = MemoryStore::with_entries(vec![malformed]);
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store));
    confirm(&mut client, full_cache_caps());
    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu { cache_slots: vec![1] }),
    );

    map_surface(&mut client, 4, 4);
    assert_eq!(
        paste_and_read_origin(&mut client, 1),
        Some([0, 0, 0, 0]),
        "the slot stays empty"
    );
}

#[test]
fn imported_entries_survive_reset_graphics_and_go_with_evict() {
    let store = MemoryStore::with_entries(vec![solid_entry(42, 4, 4, [9, 8, 7, 255])]);
    let mut client = GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store));
    confirm(&mut client, full_cache_caps());
    send(
        &mut client,
        GfxPdu::CacheImportReply(CacheImportReplyPdu { cache_slots: vec![3] }),
    );

    // MS-RDPEGFX 3.3.5.14 only resizes the output; the bitmap cache stays.
    map_surface(&mut client, 8, 8);
    map_surface(&mut client, 16, 16);
    assert_eq!(paste_and_read_origin(&mut client, 3), Some([9, 8, 7, 255]));

    send(
        &mut client,
        GfxPdu::EvictCacheEntry(EvictCacheEntryPdu { cache_slot: 3 }),
    );
    map_surface(&mut client, 16, 16);
    assert_eq!(paste_and_read_origin(&mut client, 3), Some([0, 0, 0, 0]));
}

// ============================================================================
// Save
// ============================================================================

#[test]
fn closing_saves_the_cache_most_recently_used_first() {
    let store = MemoryStore::default();
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(&mut client, full_cache_caps());
    map_surface(&mut client, 8, 8);

    let fill = |client: &mut GraphicsPipelineClient, r: u8| {
        send(
            client,
            GfxPdu::SolidFill(SolidFillPdu {
                surface_id: 1,
                fill_pixel: ironrdp_egfx::pdu::Color { b: 0, g: 0, r, xa: 0 },
                rectangles: vec![rect(0, 0, 8, 8)],
            }),
        );
    };
    let to_cache = |client: &mut GraphicsPipelineClient, cache_key: u64, cache_slot: u16, size: u16| {
        send(
            client,
            GfxPdu::SurfaceToCache(SurfaceToCachePdu {
                surface_id: 1,
                cache_key,
                cache_slot,
                source_rectangle: rect(0, 0, size, size),
            }),
        );
    };
    fill(&mut client, 11);
    to_cache(&mut client, 0xAAAA, 1, 2);
    fill(&mut client, 22);
    to_cache(&mut client, 0xBBBB, 2, 4);
    // Pasting slot 1 makes it the most recently used.
    paste_and_read_origin(&mut client, 1);

    client.close(0);

    let state = store.state();
    let saved = state.saved.as_ref().expect("the cache was saved");
    let keys: Vec<u64> = saved.iter().map(|entry| entry.cache_key).collect();
    assert_eq!(keys, [0xAAAA, 0xBBBB]);
    assert_eq!(saved[0], solid_entry(0xAAAA, 2, 2, [11, 0, 0, 255]));
    assert_eq!(saved[1], solid_entry(0xBBBB, 4, 4, [22, 0, 0, 255]));
}

#[test]
fn closing_with_an_empty_cache_keeps_what_was_stored() {
    let store = MemoryStore::with_entries(vec![solid_entry(1, 2, 2, [1, 1, 1, 255])]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    confirm(&mut client, full_cache_caps());

    client.close(0);

    assert!(store.state().saved.is_none());
}

// ============================================================================
// Round trip through the server
// ============================================================================

/// A server handler that accepts every offered entry into consecutive slots starting at 10.
struct AcceptingServer {
    offers: Arc<Mutex<Vec<CacheImportOfferPdu>>>,
}

impl ServerHandler for AcceptingServer {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}
    fn on_ready(&mut self, _negotiated: &CapabilitySet) {}
    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {}
    fn on_qoe_metrics(&mut self, _metrics: QoeMetrics) {}
    fn on_surface_created(&mut self, _surface: &Surface) {}
    fn on_surface_deleted(&mut self, _surface_id: u16) {}

    fn on_cache_import_offer(&mut self, offer: &CacheImportOfferPdu) -> Vec<u16> {
        self.offers.lock().unwrap().push(offer.clone());
        (10..).take(offer.cache_entries.len()).collect()
    }
}

/// Hand server output to the client; returns the decoded server PDUs and the client's answers.
fn server_to_client(
    messages: Vec<DvcMessage>,
    client: &mut GraphicsPipelineClient,
    decompressor: &mut Decompressor,
) -> (Vec<GfxPdu>, Vec<DvcMessage>) {
    let mut server_pdus = Vec::new();
    let mut client_responses = Vec::new();
    for message in messages {
        let wire = encode_vec(message.as_ref()).unwrap();
        let mut decompressed = Vec::new();
        decompressor.decompress(&wire, &mut decompressed).unwrap();
        server_pdus.push(GfxPdu::decode(&mut ReadCursor::new(&decompressed)).unwrap());
        client_responses.extend(client.process(0, &wire).expect("client processes server output"));
    }
    (server_pdus, client_responses)
}

/// Hand client output to the server; returns the server's answers.
fn client_to_server(server: &mut GraphicsPipelineServer, messages: Vec<DvcMessage>) -> Vec<DvcMessage> {
    messages
        .into_iter()
        .flat_map(|message| {
            server
                .process(0, &encode_vec(message.as_ref()).unwrap())
                .expect("server processes client output")
        })
        .collect()
}

#[test]
fn offer_and_reply_round_trip_through_the_server() {
    let store = MemoryStore::with_entries(vec![
        solid_entry(0x1111, 4, 4, [200, 100, 50, 255]),
        solid_entry(0x2222, 4, 4, [10, 20, 30, 255]),
    ]);
    let mut client =
        GraphicsPipelineClient::new(Box::new(NoopHandler), None).with_persistent_cache(Box::new(store.clone()));
    let offers = Arc::new(Mutex::new(Vec::new()));
    let mut server = GraphicsPipelineServer::new(Box::new(AcceptingServer {
        offers: Arc::clone(&offers),
    }));
    let mut decompressor = Decompressor::new();

    // CapabilitiesAdvertise -> CapabilitiesConfirm -> CacheImportOffer -> CacheImportReply.
    let advertise = client.start(0).unwrap();
    let confirm = client_to_server(&mut server, advertise);
    let (_, offer) = server_to_client(confirm, &mut client, &mut decompressor);
    assert_eq!(offer.len(), 1, "the client answers the confirm with an offer");
    let reply = client_to_server(&mut server, offer);
    let (reply, responses) = server_to_client(reply, &mut client, &mut decompressor);
    assert!(responses.is_empty());
    assert!(
        matches!(reply.as_slice(), [GfxPdu::CacheImportReply(reply)] if reply.cache_slots == [10, 11]),
        "{reply:?}"
    );

    let offers = offers.lock().unwrap();
    assert_eq!(
        offers[0]
            .cache_entries
            .iter()
            .map(|entry| entry.cache_key)
            .collect::<Vec<_>>(),
        [0x1111, 0x2222]
    );
    assert_eq!(store.state().loads, [0x1111, 0x2222]);

    // The server pastes from the imported slots without uploading anything.
    map_surface(&mut client, 8, 8);
    assert_eq!(paste_and_read_origin(&mut client, 10), Some([200, 100, 50, 255]));
    assert_eq!(paste_and_read_origin(&mut client, 11), Some([10, 20, 30, 255]));

    // The imported entries are saved again when the channel closes.
    client.close(0);
    let saved: BTreeMap<u64, PersistentCacheEntry> = store
        .state()
        .saved
        .clone()
        .unwrap()
        .into_iter()
        .map(|entry| (entry.cache_key, entry))
        .collect();
    assert_eq!(saved[&0x1111], solid_entry(0x1111, 4, 4, [200, 100, 50, 255]));
    assert_eq!(saved[&0x2222], solid_entry(0x2222, 4, 4, [10, 20, 30, 255]));
}
