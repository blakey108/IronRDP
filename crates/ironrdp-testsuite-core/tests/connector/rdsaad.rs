//! RDS AAD Auth in the client connector: negotiation, the proof-of-possession key
//! identifiers, and the post-TLS JSON exchange.
//!
//! The RSA key below was generated with OpenSSL. The JWK parameters, the RFC 7638 thumbprint
//! and the `req_cnf` value were computed from it with OpenSSL and Python, independently of
//! the connector.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ironrdp_connector::rdsaad::{RdsAadAuthFailure, RdsAadCredentials, RdsAadSequence, pop_key_req_cnf};
use ironrdp_connector::{
    ClientConnector, ClientConnectorState, ConnectorErrorKind, Credentials, Sequence as _, Written,
};
use ironrdp_core::{WriteBuf, decode, encode_vec};
use ironrdp_pdu::nego;
use ironrdp_pdu::x224::X224;
use picky::jose::jws::Jws;
use picky::key::PublicKey;
use rstest::rstest;

/// PKCS#8 DER, base64.
const POP_KEY: &str = concat!(
    "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCg1vUTcZB1YTbCWXwpHfOvvxyccEefKLAQFXNAQ22zfcZe",
    "F55EELHCT3eublEYwEhpzS5y3Ar/og9s7AaSJJiVyc5Bq1rLSEKRL2yQKQZDRCEfl/4ohlwisPc4CnC3ZqZrPxGzX97XwlRv",
    "ZDbAcLW9lddiqbvFJ0HcGI/wFaWdufDjH/km6omDa5JChfDQf0TSziO9sK08hi6oT0ofEAdRipaKNwLdkmv0B4Aa6bw96YLg",
    "LStYk0XvjMJh89BT71uyzcd++jYMjRu8631eV4HPT1OVmIDdfQhQNI13vtDqSTGD6ge9EZ+M8xNqFXG5HJpYU0rpCTGXoI0w",
    "/WZXRQuRAgMBAAECggEAAXFsSXxfW24/24aKAqRuJ0mDrDOlsNvgmIRYwF7agR3DxdxYYyvzLFciL8J1n8851xksLYOj5Z9C",
    "nCuR84W4iL+CxjwPWHuRfVP7mV+E2t2lnm7yrNJnyTflP+94C200WQn70qwth6oKr+UqSJe6SbUqdYwoTyCHCr59rB3FmqCo",
    "0Gi7yjCbn7Ys3mgYwZRC/xHboHVR84xl1YGb8lL7KwMOLeOjiGbzEneQjUCX2rb5/EtBCqsLlPe8S29EgNK1i2o1GtaGunfj",
    "e7DzYClb8QLrBt0J8YvnRUIV80gM0ngWVVBZmsLeHv8a/PlMOUrrg6LlghSzorSmdrmAFo+AAQKBgQDNee8c2+eoQfLt7mHD",
    "LskztaSqKc2H3kMWdHu74g8TkgGgu1lXMAPMeyi6jWYHCtIbVDdvk7Luh8fcuWepx+fAQ0VCFgK28ahIJv7JtjBAg2NG2ti2",
    "5Aw6rr5G+bVQPsE8v61wMa3ZFcBIPtP/O5pSOzTbECW6Yl4r9q7cb3zFkQKBgQDIY0rleB30RemWJLNYcIGSRpYD45V4lPeb",
    "uin/dvChB7W4ccZpe7bn/48XDCUvZ9hn3UVCxNangjBR/hJF2r1u+Tgvnx0goZdZ3SMYLe29pvn3Dk/7WikL0aIoSqms2ifT",
    "EW+Q9yguJNBn3oFcduprkn24kIZrRozBSEEfpcjmAQKBgCmW4ZgUxpne513TX0RTZvQpcu/iUDCNA0U7RIQt6GFMrzmqUhX8",
    "qPgtgXvo/Nq2DPrtG99gpNn077Y0bNAHn1fiBdWvgNMorKgWBIItdyzHq0N/LlguZm9GSUsyZcmdOh2iqmdIFukWpvm4hc5m",
    "cQDt7O/eH9TO7kK1xoTlp/qRAoGAL8v/QVy1SF6kqLt4S9AKF+nePm86Z9igdvSgr96e5sn//k6lFOP9bLj4WHb9J6GNbLjK",
    "nHJ3AN18yRHhKehS5vVrAXDicbYZc8XbaRkLbAE3/kXbrgiDYd5O321vq1sdxKqUAe/yFY+SZjQchwGWSLJYan2OkmfxiujB",
    "H9tbYAECgYBhE2J+gFDIHkVN2DtWUhMW3INn2ABGBUFZqP1U2nr4PhVx13h24lE1dch4YZ8iFT0zjAbbRMTPQRekMMJH7xem",
    "4TPuacJgcC7ec4hZmXyMsNMfI572SJUBAflqvvM4GwDeIP+T505WSsQAkCdd1Q91feJgDFAPq4tFhLbb1E9PeA=="
);

/// SubjectPublicKeyInfo DER, base64.
const POP_PUBLIC_KEY: &str = concat!(
    "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAoNb1E3GQdWE2wll8KR3zr78cnHBHnyiwEBVzQENts33GXheeRBCx",
    "wk93rm5RGMBIac0uctwK/6IPbOwGkiSYlcnOQatay0hCkS9skCkGQ0QhH5f+KIZcIrD3OApwt2amaz8Rs1/e18JUb2Q2wHC1",
    "vZXXYqm7xSdB3BiP8BWlnbnw4x/5JuqJg2uSQoXw0H9E0s4jvbCtPIYuqE9KHxAHUYqWijcC3ZJr9AeAGum8PemC4C0rWJNF",
    "74zCYfPQU+9bss3Hfvo2DI0bvOt9XleBz09TlZiA3X0IUDSNd77Q6kkxg+oHvRGfjPMTahVxuRyaWFNK6Qkxl6CNMP1mV0UL",
    "kQIDAQAB"
);

// oracle: OpenSSL `rsa -modulus` and Python base64url of the unsigned big-endian modulus
const POP_KEY_N: &str = "oNb1E3GQdWE2wll8KR3zr78cnHBHnyiwEBVzQENts33GXheeRBCxwk93rm5RGMBIac0uctwK_6IPbOwGkiSYlcnOQatay0hCkS9skCkGQ0QhH5f-KIZcIrD3OApwt2amaz8Rs1_e18JUb2Q2wHC1vZXXYqm7xSdB3BiP8BWlnbnw4x_5JuqJg2uSQoXw0H9E0s4jvbCtPIYuqE9KHxAHUYqWijcC3ZJr9AeAGum8PemC4C0rWJNF74zCYfPQU-9bss3Hfvo2DI0bvOt9XleBz09TlZiA3X0IUDSNd77Q6kkxg-oHvRGfjPMTahVxuRyaWFNK6Qkxl6CNMP1mV0ULkQ";
const POP_KEY_E: &str = "AQAB";
// oracle: RFC 7638 thumbprint computed in Python: base64url(SHA-256('{"e":"AQAB","kty":"RSA","n":"<n>"}'))
const POP_KEY_THUMBPRINT: &str = "xSX8RrzvaX-nEIO2PGzJt1O24fY2J-XzF7fTZutdx1Q";
// oracle: spec MS-RDPBCGR §4.11.4, base64url('{"kid":"<thumbprint>"}') computed in Python
const POP_KEY_REQ_CNF: &str = "eyJraWQiOiJ4U1g4UnJ6dmFYLW5FSU8yUEd6SnQxTzI0ZlkySi1YekY3ZlRadXRkeDFRIn0";

const ACCESS_TOKEN: &str = "access-token-under-test";
const AAD_NONCE: &str = "aad-nonce-under-test";
const TS_NONCE: &str = "ts-nonce-under-test";
const HOSTNAME: &str = "DESKTOP-TEST";
const UNIX_TIME_SECS: u64 = 1_700_000_000;

fn credentials() -> RdsAadCredentials {
    RdsAadCredentials {
        username: "user@example.com".to_owned(),
        access_token: ACCESS_TOKEN.to_owned(),
        aad_nonce: AAD_NONCE.to_owned(),
        pop_key: STANDARD.decode(POP_KEY).unwrap(),
        hostname: HOSTNAME.to_owned(),
    }
}

fn null_terminated(json: &str) -> Vec<u8> {
    let mut pdu = json.as_bytes().to_vec();
    pdu.push(0);
    pdu
}

/// Runs the server nonce step and returns the decoded Authentication Request PDU.
fn authentication_request(sequence: &mut RdsAadSequence) -> serde_json::Value {
    let mut output = WriteBuf::new();
    let written = sequence
        .step(
            &null_terminated(&format!(r#"{{"ts_nonce":"{TS_NONCE}"}}"#)),
            &mut output,
        )
        .unwrap();

    let pdu = &output.filled()[..written.size().unwrap()];
    let (json, terminator) = pdu.split_at(pdu.len() - 1);
    assert_eq!(terminator, [0], "RDS AAD Auth PDUs are null-terminated");

    serde_json::from_slice(json).unwrap()
}

#[test]
fn req_cnf_is_the_rfc7638_thumbprint_of_the_pop_key() {
    let req_cnf = pop_key_req_cnf(&STANDARD.decode(POP_KEY).unwrap()).unwrap();

    assert_eq!(req_cnf, POP_KEY_REQ_CNF);
}

// oracle: spec MS-RDPBCGR §2.2.18.2.1 (RDP Assertion) and §4.11.4 (kid, cnf)
#[test]
fn authentication_request_carries_a_signed_assertion_over_both_nonces() {
    let mut sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();

    let request = authentication_request(&mut sequence);
    let assertion = request["rdp_assertion"].as_str().unwrap();

    let public_key = PublicKey::from_der(&STANDARD.decode(POP_PUBLIC_KEY).unwrap()).unwrap();
    let jws = Jws::decode(assertion, &public_key).expect("assertion is signed by the proof-of-possession key");

    assert_eq!(jws.header.kid.as_deref(), Some(POP_KEY_REQ_CNF));

    let payload: serde_json::Value = serde_json::from_slice(&jws.payload).unwrap();
    assert_eq!(payload["ts"], UNIX_TIME_SECS.to_string());
    assert_eq!(payload["at"], ACCESS_TOKEN);
    assert_eq!(
        payload["u"],
        format!("ms-device-service://termsrv.wvd.microsoft.com/name/{HOSTNAME}")
    );
    assert_eq!(payload["nonce"], TS_NONCE);
    assert_eq!(payload["cnf"]["jwk"]["kty"], "RSA");
    assert_eq!(payload["cnf"]["jwk"]["e"], POP_KEY_E);
    assert_eq!(payload["cnf"]["jwk"]["n"], POP_KEY_N);
    assert_eq!(payload["client_claims"], format!(r#"{{"aad_nonce":"{AAD_NONCE}"}}"#));

    let header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(assertion.split('.').next().unwrap()).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["kid"], POP_KEY_THUMBPRINT_KID());
}

// oracle: spec MS-RDPBCGR §2.2.18.2.1 (RDP Assertion), member order of the JWS Payload format
#[test]
fn assertion_payload_members_follow_the_specification_order() {
    let mut sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();

    let request = authentication_request(&mut sequence);
    let assertion = request["rdp_assertion"].as_str().unwrap();
    let payload = URL_SAFE_NO_PAD.decode(assertion.split('.').nth(1).unwrap()).unwrap();
    let payload = String::from_utf8(payload).unwrap();

    let expected = [
        r#"{"ts":"#,
        r#","at":"#,
        r#","u":"#,
        r#","nonce":"#,
        r#","cnf":{"jwk":{"kty":"RSA","e":"#,
        r#","n":"#,
        r#"}},"client_claims":"#,
    ];
    let mut rest = payload.as_str();
    for member in expected {
        let (_, after) = rest
            .split_once(member)
            .unwrap_or_else(|| panic!("{member} missing or out of order"));
        rest = after;
    }
}

// oracle: spec MS-RDPBCGR §4.11.4, the two forms of the access token scope
#[rstest]
#[case::hostname("DESKTOP-TEST", "ms-device-service://termsrv.wvd.microsoft.com/name/DESKTOP-TEST")]
#[case::device_id(
    "5d9d9c9f-1805-42e3-8c87-15208f7e175d",
    "ms-device-service://termsrv.wvd.microsoft.com/id/5d9d9c9f-1805-42e3-8c87-15208f7e175d"
)]
fn assertion_names_the_resource_the_token_was_acquired_for(#[case] target: &str, #[case] resource_uri: &str) {
    let credentials = RdsAadCredentials {
        hostname: target.to_owned(),
        ..credentials()
    };
    let mut sequence = RdsAadSequence::init(credentials, UNIX_TIME_SECS).unwrap();

    let request = authentication_request(&mut sequence);
    let assertion = request["rdp_assertion"].as_str().unwrap();
    let payload = URL_SAFE_NO_PAD.decode(assertion.split('.').nth(1).unwrap()).unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();

    assert_eq!(payload["u"], resource_uri);
}

/// The header `kid` is the same opaque value as `req_cnf`.
#[expect(non_snake_case, reason = "reads as the constant it derives")]
fn POP_KEY_THUMBPRINT_KID() -> String {
    let decoded: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(POP_KEY_REQ_CNF).unwrap()).unwrap();
    assert_eq!(decoded["kid"], POP_KEY_THUMBPRINT);
    POP_KEY_REQ_CNF.to_owned()
}

#[test]
fn successful_authentication_result_finishes_the_sequence() {
    let mut sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();
    authentication_request(&mut sequence);

    let mut output = WriteBuf::new();
    let written = sequence
        .step(&null_terminated(r#"{"authentication_result":0}"#), &mut output)
        .unwrap();

    assert_eq!(written, Written::Nothing);
    assert!(sequence.next_pdu_hint().is_none());
}

// oracle: spec MS-RDPBCGR §2.2.18.3 (HRESULT), MS-ERREF for the code values
#[rstest]
#[case::e_accessdenied_as_signed_int32("-2147024891", RdsAadAuthFailure::E_ACCESSDENIED)]
#[case::sec_e_invalid_token_as_unsigned("2148074248", RdsAadAuthFailure::SEC_E_INVALID_TOKEN)]
#[case::status_logon_failure_as_hresult("3489661037", RdsAadAuthFailure::STATUS_LOGON_FAILURE)]
#[case::status_logon_failure_as_ntstatus("3221225581", RdsAadAuthFailure::STATUS_LOGON_FAILURE)]
#[case::code_as_decimal_string("\"2147942405\"", RdsAadAuthFailure::E_ACCESSDENIED)]
fn failed_authentication_result_is_reported(#[case] json_value: &str, #[case] expected: RdsAadAuthFailure) {
    let mut sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();
    authentication_request(&mut sequence);

    let mut output = WriteBuf::new();
    let error = sequence
        .step(
            &null_terminated(&format!(r#"{{"authentication_result":{json_value}}}"#)),
            &mut output,
        )
        .unwrap_err();

    match error.kind() {
        ConnectorErrorKind::RdsAad(failure) => assert_eq!(*failure, expected),
        other => panic!("unexpected error kind: {other}"),
    }
}

#[test]
fn pdu_hint_waits_for_the_null_terminator() {
    let sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();
    let hint = sequence.next_pdu_hint().unwrap();

    assert_eq!(hint.find_size(br#"{"ts_nonce":"abc"#).unwrap(), None);
    assert_eq!(
        hint.find_size(b"{\"ts_nonce\":\"abc\"}\0trailing").unwrap(),
        Some((true, 19))
    );
}

#[test]
fn pdu_hint_rejects_unterminated_oversized_messages() {
    let sequence = RdsAadSequence::init(credentials(), UNIX_TIME_SECS).unwrap();
    let hint = sequence.next_pdu_hint().unwrap();

    let oversized = vec![b'x'; 64 * 1024];

    assert!(hint.find_size(&oversized).is_err());
}

// oracle: spec MS-RDPBCGR §5.4.5.4, the client advertises PROTOCOL_RDSAAD in RDP_NEG_REQ
#[test]
fn rdsaad_credentials_negotiate_rdsaad_only_and_run_the_exchange_after_tls() {
    let mut config = super::test_config();
    config.credentials = Credentials::RdsAad(credentials());
    config.enable_credssp = true;
    let mut connector = ClientConnector::new(config, "127.0.0.1:12345".parse().unwrap());

    let mut output = WriteBuf::new();
    connector.step_no_input(&mut output).unwrap();
    let request: X224<nego::ConnectionRequest> = decode(output.filled()).unwrap();
    assert_eq!(request.0.protocol, nego::SecurityProtocol::RDSAAD);

    let confirm = encode_vec(&X224(nego::ConnectionConfirm::Response {
        flags: nego::ResponseFlags::empty(),
        protocol: nego::SecurityProtocol::RDSAAD,
    }))
    .unwrap();
    output.clear();
    connector.step(&confirm, None, &mut output).unwrap();
    assert!(matches!(
        connector.state,
        ClientConnectorState::EnhancedSecurityUpgrade {
            selected_protocol: nego::SecurityProtocol::RDSAAD
        }
    ));

    connector.mark_security_upgrade_as_done();
    assert!(connector.should_perform_rdsaad());
    assert!(!connector.should_perform_credssp());

    connector.mark_rdsaad_as_done();
    assert!(matches!(
        connector.state,
        ClientConnectorState::BasicSettingsExchangeSendInitial { .. }
    ));
}
