use ironrdp_graphics::alpha::{AlphaDecodeError, decode};

/// `alphaSig` (0x414C, "LA" on the wire) followed by `compressed`.
fn header(compressed: u16) -> Vec<u8> {
    let mut stream = vec![0x4C, 0x41];
    stream.extend_from_slice(&compressed.to_le_bytes());
    stream
}

#[test]
fn raw_stream_carries_one_byte_per_pixel() {
    let mut stream = header(0);
    stream.extend_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);

    assert_eq!(decode(&stream, 3, 2), Ok(vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
}

#[test]
fn raw_stream_ignores_trailing_bytes() {
    let mut stream = header(0);
    stream.extend_from_slice(&[1, 2, 3, 4, 0xEE]);

    assert_eq!(decode(&stream, 2, 2), Ok(vec![1, 2, 3, 4]));
}

#[test]
fn raw_stream_shorter_than_the_bitmap_is_truncated() {
    let mut stream = header(0);
    stream.extend_from_slice(&[1, 2, 3]);

    assert_eq!(decode(&stream, 2, 2), Err(AlphaDecodeError::Truncated));
}

#[test]
fn rle_one_byte_run_lengths() {
    let mut stream = header(1);
    // runValue 0xFF x 2, runValue 0x00 x 1, runValue 0x7F x 3.
    stream.extend_from_slice(&[0xFF, 2, 0x00, 1, 0x7F, 3]);

    assert_eq!(decode(&stream, 3, 2), Ok(vec![0xFF, 0xFF, 0x00, 0x7F, 0x7F, 0x7F]));
}

#[test]
fn rle_two_byte_run_length_behind_the_0xff_escape() {
    let mut stream = header(1);
    // runLengthFactor1 0xFF, runLengthFactor2 300.
    stream.extend_from_slice(&[0xAB, 0xFF]);
    stream.extend_from_slice(&300u16.to_le_bytes());

    let alpha = decode(&stream, 20, 15).unwrap();
    assert_eq!(alpha, vec![0xAB; 300]);
}

#[test]
fn rle_four_byte_run_length_behind_the_0xffff_escape() {
    let mut stream = header(1);
    // runLengthFactor1 0xFF, runLengthFactor2 0xFFFF, runLengthFactor3 70000.
    stream.extend_from_slice(&[0x40, 0xFF, 0xFF, 0xFF]);
    stream.extend_from_slice(&70_000u32.to_le_bytes());

    let alpha = decode(&stream, 350, 200).unwrap();
    assert_eq!(alpha.len(), 70_000);
    assert!(alpha.iter().all(|value| *value == 0x40));
}

#[test]
fn rle_zero_length_runs_are_allowed() {
    let mut stream = header(1);
    stream.extend_from_slice(&[0x10, 0, 0x20, 2]);

    assert_eq!(decode(&stream, 2, 1), Ok(vec![0x20, 0x20]));
}

#[test]
fn rle_run_past_the_bitmap_is_rejected() {
    let mut stream = header(1);
    stream.extend_from_slice(&[0x10, 5]);

    assert_eq!(
        decode(&stream, 2, 2),
        Err(AlphaDecodeError::PixelCountMismatch {
            expected: 4,
            decoded: 5
        })
    );
}

#[test]
fn rle_stream_ending_before_the_bitmap_is_full_is_truncated() {
    let mut stream = header(1);
    stream.extend_from_slice(&[0x10, 3]);

    assert_eq!(decode(&stream, 2, 2), Err(AlphaDecodeError::Truncated));
}

#[test]
fn rle_escape_without_its_wider_length_is_truncated() {
    let mut stream = header(1);
    stream.extend_from_slice(&[0x10, 0xFF, 0x01]);

    assert_eq!(decode(&stream, 2, 2), Err(AlphaDecodeError::Truncated));
}

#[test]
fn wrong_signature_is_rejected() {
    assert_eq!(
        decode(&[0x41, 0x4C, 0, 0, 1], 1, 1),
        Err(AlphaDecodeError::InvalidSignature(0x4C41))
    );
}

#[test]
fn missing_header_is_truncated() {
    assert_eq!(decode(&[0x4C, 0x41, 0], 1, 1), Err(AlphaDecodeError::Truncated));
}
