//! RDS AAD Auth: Microsoft Entra ID token-based user authentication.
//!
//! RDS AAD Auth is a variation of Enhanced RDP Security where the user is authenticated by
//! exchanging JSON messages directly after the TLS handshake, instead of running CredSSP.
//! The client proves possession of an Entra ID access token that is bound to an RSA key pair
//! (a proof-of-possession key), and the server validates the assertion against Entra ID.
//!
//! This module holds the protocol-level state machine. Acquiring the access token and the
//! Entra ID nonce requires HTTPS calls to the Microsoft identity platform, which is left to the
//! application because the connector does no I/O.
//!
//! See [MS-RDPBCGR] section "5.4.5.4 RDS AAD Auth Security"[^rdsaad] and the PDUs in section
//! "2.2.18 RDS AAD Auth PDUs"[^pdus].
//!
//! [^rdsaad]: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/dc43f040-d75d-49a9-90c6-0c9999281136>
//! [^pdus]: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/43e8dd31-7324-4578-bf10-5eb93dd6f4df>

use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ironrdp_core::{DecodeResult, WriteBuf, other_err};
use ironrdp_pdu::PduHint;
use picky::hash::HashAlgorithm;
use picky::jose::jws::{Jws, JwsAlg, JwsHeader};
use picky::key::PrivateKey;
use picky_asn1_x509::{PublicKey as Asn1PublicKey, SubjectPublicKeyInfo};
use tracing::debug;

use crate::{ConnectorError, ConnectorErrorKind, ConnectorResult, Written, custom_err, general_err, reason_err};

/// Prefix of the token audience; the target follows as `id/<device ID>` or `name/<hostname>`.
const TOKEN_AUDIENCE_PREFIX: &str = "ms-device-service://termsrv.wvd.microsoft.com/";

/// Size of the RSA proof-of-possession key, as required by [MS-RDPBCGR] section 4.11.4.
const POP_KEY_BITS: usize = 2048;

/// Upper bound for a single RDS AAD Auth PDU.
///
/// The PDUs carry no length prefix, so without a bound a misbehaving server could make the
/// client buffer indefinitely while waiting for the terminating null byte.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// HRESULT reported in the Authentication Result PDU on success.
const S_OK: u32 = 0;

/// Credentials for RDS AAD Auth.
///
/// The access token must be requested with the `req_cnf` parameter returned by
/// [`pop_key_req_cnf`] for the same `pop_key`, and for the scope
/// `ms-device-service://termsrv.wvd.microsoft.com/name/<hostname>/user_impersonation`, or
/// `ms-device-service://termsrv.wvd.microsoft.com/id/<device ID>/user_impersonation` when
/// `hostname` holds the Entra ID device ID of the target.
#[derive(Clone)]
pub struct RdsAadCredentials {
    /// User principal name sent in the Client Info PDU.
    ///
    /// The server derives the user from the access token, so this may be empty.
    pub username: String,
    /// Entra ID access token bound to `pop_key`.
    pub access_token: String,
    /// Entra ID nonce obtained from the token endpoint with `grant_type=srv_challenge`.
    pub aad_nonce: String,
    /// DER-encoded PKCS#8 RSA private key the access token is bound to.
    pub pop_key: Vec<u8>,
    /// Entra ID device name or device ID of the target, as used in the token scope.
    ///
    /// A value in GUID form is taken as the device ID.
    pub hostname: String,
}

impl fmt::Debug for RdsAadCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdsAadCredentials")
            .field("username", &self.username)
            .field("access_token", &"<redacted>")
            .field("aad_nonce", &self.aad_nonce)
            .field("pop_key", &"<redacted>")
            .field("hostname", &self.hostname)
            .finish()
    }
}

/// Authentication failure reported by the server in the Authentication Result PDU.
///
/// The code is an HRESULT; an NTSTATUS error sent as-is is normalized to its HRESULT form
/// (`HRESULT_FROM_NT`) so the constants below match either spelling.
///
/// See [MS-RDPBCGR] section "2.2.18.3 Authentication Result PDU"[^result].
///
/// [^result]: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/fefe9826-e4a0-4763-9c9a-ebacc99a8b5b>
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RdsAadAuthFailure(u32);

impl RdsAadAuthFailure {
    pub const SEC_E_INVALID_TOKEN: Self = Self(0x8009_0308);
    pub const E_ACCESSDENIED: Self = Self(0x8007_0005);
    pub const STATUS_LOGON_FAILURE: Self = Self(0xD000_006D);
    pub const STATUS_NO_LOGON_SERVERS: Self = Self(0xD000_005E);
    pub const STATUS_INVALID_LOGON_HOURS: Self = Self(0xD000_006F);
    pub const STATUS_INVALID_WORKSTATION: Self = Self(0xD000_0070);
    pub const STATUS_PASSWORD_EXPIRED: Self = Self(0xD000_0071);
    pub const STATUS_ACCOUNT_DISABLED: Self = Self(0xD000_0072);

    /// Severity and facility bits of an NTSTATUS error code.
    const NTSTATUS_ERROR_MASK: u32 = 0xF000_0000;
    const NTSTATUS_ERROR: u32 = 0xC000_0000;
    /// Set by `HRESULT_FROM_NT` to mark an NTSTATUS carried in an HRESULT.
    const FACILITY_NT_BIT: u32 = 0x1000_0000;

    fn new(code: u32) -> Self {
        if code & Self::NTSTATUS_ERROR_MASK == Self::NTSTATUS_ERROR {
            Self(code | Self::FACILITY_NT_BIT)
        } else {
            Self(code)
        }
    }

    /// The HRESULT value sent by the server.
    pub fn code(self) -> u32 {
        self.0
    }
}

impl core::error::Error for RdsAadAuthFailure {}

impl fmt::Display for RdsAadAuthFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::SEC_E_INVALID_TOKEN => write!(f, "server rejected the access token"),
            Self::E_ACCESSDENIED => write!(f, "access denied"),
            Self::STATUS_LOGON_FAILURE => write!(f, "logon failure"),
            Self::STATUS_NO_LOGON_SERVERS => write!(f, "no logon server available"),
            Self::STATUS_INVALID_LOGON_HOURS => write!(f, "logon outside of the permitted hours"),
            Self::STATUS_INVALID_WORKSTATION => write!(f, "logon not permitted from this workstation"),
            Self::STATUS_PASSWORD_EXPIRED => write!(f, "password expired"),
            Self::STATUS_ACCOUNT_DISABLED => write!(f, "account disabled"),
            Self(code) => write!(f, "authentication failed (code: 0x{code:08X})"),
        }
    }
}

/// Generates a 2048-bit RSA proof-of-possession key, DER-encoded as PKCS#8.
///
/// This is slow in debug builds and in WebAssembly; browsers should prefer the Web Crypto API and
/// export the key as PKCS#8.
pub fn generate_pop_key() -> ConnectorResult<Vec<u8>> {
    let key = PrivateKey::generate_rsa(POP_KEY_BITS).map_err(|e| custom_err!("generate RSA key", e))?;
    key.to_pkcs8().map_err(|e| custom_err!("encode RSA key", e))
}

/// Computes the `req_cnf` token request parameter binding an access token to `pop_key`.
///
/// `pop_key` is a DER-encoded PKCS#8 RSA private key.
/// The value is the base64url encoding of `{"kid":"<thumbprint>"}` where the thumbprint is the
/// RFC 7638 JWK thumbprint of the public key, as required by [MS-RDPBCGR] section 4.11.4.
pub fn pop_key_req_cnf(pop_key: &[u8]) -> ConnectorResult<String> {
    let key = PopKey::from_pkcs8(pop_key)?;
    Ok(key.req_cnf())
}

/// RSA key pair with the derived identifiers used in the token request and the assertion.
struct PopKey {
    private_key: PrivateKey,
    /// Base64url-encoded public exponent.
    e: String,
    /// Base64url-encoded modulus.
    n: String,
}

impl fmt::Debug for PopKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PopKey")
            .field("kid", &self.kid())
            .finish_non_exhaustive()
    }
}

impl PopKey {
    fn from_pkcs8(pkcs8: &[u8]) -> ConnectorResult<Self> {
        let private_key = PrivateKey::from_pkcs8(pkcs8).map_err(|e| custom_err!("decode RSA key", e))?;
        let public_key_der = private_key
            .to_public_key()
            .and_then(|public_key| public_key.to_der())
            .map_err(|e| custom_err!("derive RSA public key", e))?;
        let public_key_info: SubjectPublicKeyInfo =
            picky_asn1_der::from_bytes(&public_key_der).map_err(|e| custom_err!("decode RSA public key", e))?;
        let Asn1PublicKey::Rsa(rsa) = public_key_info.subject_public_key else {
            return Err(general_err!("proof-of-possession key is not an RSA key"));
        };

        // JWK encodes RSA parameters as unsigned big-endian integers without leading zeros.
        Ok(Self {
            private_key,
            e: URL_SAFE_NO_PAD.encode(rsa.0.public_exponent.as_unsigned_bytes_be()),
            n: URL_SAFE_NO_PAD.encode(rsa.0.modulus.as_unsigned_bytes_be()),
        })
    }

    /// RFC 7638 thumbprint of the public key, base64url-encoded.
    fn thumbprint(&self) -> String {
        // RFC 7638 requires the required members in lexicographic order with no whitespace.
        let canonical = format!(r#"{{"e":"{}","kty":"RSA","n":"{}"}}"#, self.e, self.n);
        URL_SAFE_NO_PAD.encode(HashAlgorithm::SHA2_256.digest(canonical.as_bytes()))
    }

    /// Key identifier shared by the `req_cnf` parameter and the assertion header.
    fn kid(&self) -> String {
        let json = serde_json::json!({ "kid": self.thumbprint() });
        URL_SAFE_NO_PAD.encode(json.to_string())
    }

    fn req_cnf(&self) -> String {
        self.kid()
    }
}

/// Finds the end of a null-terminated RDS AAD Auth PDU.
#[derive(Clone, Copy, Debug)]
struct RdsAadMessageHint;

const RDSAAD_MESSAGE_HINT: RdsAadMessageHint = RdsAadMessageHint;

impl PduHint for RdsAadMessageHint {
    fn find_size(&self, bytes: &[u8]) -> DecodeResult<Option<(bool, usize)>> {
        match bytes.iter().position(|byte| *byte == 0) {
            Some(terminator) => Ok(Some((true, terminator + 1))),
            None if bytes.len() < MAX_MESSAGE_SIZE => Ok(None),
            None => Err(other_err!(
                "RdsAadMessageHint",
                "RDS AAD Auth PDU exceeds the maximum size"
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RdsAadState {
    WaitServerNonce,
    WaitAuthenticationResult,
    Finished,
}

/// State machine for the RDS AAD Auth exchange performed after the TLS handshake.
///
/// The server speaks first, so the driver reads a PDU with [`Self::next_pdu_hint`] and feeds it
/// to [`Self::step`] until no hint is returned.
#[derive(Debug)]
pub struct RdsAadSequence {
    credentials: RdsAadCredentials,
    pop_key: PopKey,
    unix_time_secs: u64,
    state: RdsAadState,
}

impl RdsAadSequence {
    /// `unix_time_secs` is the current time, sent as the `ts` claim of the assertion.
    pub fn init(credentials: RdsAadCredentials, unix_time_secs: u64) -> ConnectorResult<Self> {
        let pop_key = PopKey::from_pkcs8(&credentials.pop_key)?;

        Ok(Self {
            credentials,
            pop_key,
            unix_time_secs,
            state: RdsAadState::WaitServerNonce,
        })
    }

    pub fn next_pdu_hint(&self) -> Option<&dyn PduHint> {
        match self.state {
            RdsAadState::WaitServerNonce | RdsAadState::WaitAuthenticationResult => Some(&RDSAAD_MESSAGE_HINT),
            RdsAadState::Finished => None,
        }
    }

    /// Processes one server PDU and writes the client response, if any.
    pub fn step(&mut self, input: &[u8], output: &mut WriteBuf) -> ConnectorResult<Written> {
        let message = decode_message(input)?;

        let (written, next_state) = match self.state {
            RdsAadState::WaitServerNonce => {
                let ts_nonce = message
                    .get("ts_nonce")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| general_err!("server nonce PDU has no ts_nonce"))?;

                debug!("Received server nonce");

                let assertion = self.assertion(ts_nonce)?;
                let request = serde_json::json!({ "rdp_assertion": assertion });
                let written = write_message(&request, output)?;

                debug!("Sent authentication request");

                (written, RdsAadState::WaitAuthenticationResult)
            }
            RdsAadState::WaitAuthenticationResult => {
                let result = message
                    .get("authentication_result")
                    .and_then(authentication_result_code)
                    .ok_or_else(|| general_err!("authentication result PDU has no authentication_result"))?;

                debug!(code = result, "Received authentication result");

                if result != S_OK {
                    return Err(ConnectorError::new(
                        "RDS AAD Auth",
                        ConnectorErrorKind::RdsAad(RdsAadAuthFailure::new(result)),
                    ));
                }

                (Written::Nothing, RdsAadState::Finished)
            }
            RdsAadState::Finished => return Err(general_err!("RDS AAD Auth sequence is already done")),
        };

        self.state = next_state;

        Ok(written)
    }

    /// Builds the RDP Assertion: an RS256 JWS over the access token and both nonces.
    ///
    /// See [MS-RDPBCGR] sections "2.2.18.2.1 RDP Assertion"[^assertion] and "4.11.4 Acquiring an
    /// RDP Access Token"[^token].
    ///
    /// [^assertion]: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/ba819b6b-257a-466f-b8e5-f262d78677f7>
    /// [^token]: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/e967ebeb-9e9f-443e-857a-5208802943c2>
    fn assertion(&self, ts_nonce: &str) -> ConnectorResult<String> {
        // The aad_nonce travels as a JSON document embedded in a string.
        let client_claims = serde_json::json!({ "aad_nonce": self.credentials.aad_nonce }).to_string();

        // The token scope names the target by its device ID or by its hostname, and the
        // resource URI has to name it the same way.
        let target = &self.credentials.hostname;
        let form = if is_device_id(target) { "id" } else { "name" };
        let resource_uri = format!("{TOKEN_AUDIENCE_PREFIX}{form}/{target}");

        // The members are written in the order the specification lists them, as the Microsoft
        // clients send them, instead of through a map, which would sort the keys.
        // Each value goes through serde_json so that it is escaped as a JSON string.
        let string = |value: &str| serde_json::Value::from(value).to_string();
        let payload = format!(
            r#"{{"ts":{ts},"at":{at},"u":{u},"nonce":{nonce},"cnf":{{"jwk":{{"kty":"RSA","e":{e},"n":{n}}}}},"client_claims":{client_claims}}}"#,
            ts = string(&self.unix_time_secs.to_string()),
            at = string(&self.credentials.access_token),
            u = string(&resource_uri),
            nonce = string(ts_nonce),
            e = string(&self.pop_key.e),
            n = string(&self.pop_key.n),
            client_claims = string(&client_claims),
        );

        let jws = Jws {
            header: JwsHeader {
                kid: Some(self.pop_key.kid()),
                ..JwsHeader::new(JwsAlg::RS256)
            },
            payload: payload.into_bytes(),
        };

        return jws
            .encode(&self.pop_key.private_key)
            .map_err(|e| custom_err!("sign RDP assertion", e));

        /// Whether `target` is in GUID form, which a computer name is too short to be.
        fn is_device_id(target: &str) -> bool {
            const HYPHENS: [usize; 4] = [8, 13, 18, 23];

            target.len() == 36
                && target.bytes().enumerate().all(|(index, byte)| {
                    if HYPHENS.contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        }
    }
}

/// Decodes a null-terminated JSON PDU into an object.
fn decode_message(input: &[u8]) -> ConnectorResult<serde_json::Map<String, serde_json::Value>> {
    let json = input.strip_suffix(&[0]).unwrap_or(input);
    let json = core::str::from_utf8(json).map_err(|e| custom_err!("RDS AAD Auth PDU is not UTF-8", e))?;

    match serde_json::from_str(json).map_err(|e| custom_err!("RDS AAD Auth PDU is not JSON", e))? {
        serde_json::Value::Object(object) => Ok(object),
        _ => Err(reason_err!("RDS AAD Auth", "PDU is not a JSON object")),
    }
}

/// Writes a JSON PDU followed by the terminating null byte.
fn write_message(message: &serde_json::Value, output: &mut WriteBuf) -> ConnectorResult<Written> {
    let json = message.to_string();
    output.write_slice(json.as_bytes());
    output.write_u8(0);
    Written::from_size(json.len() + 1 /* null terminator */)
}

/// The spec types the result as a 32-bit integer; Windows sends it as a JSON number and some
/// implementations as a decimal string, so accept both.
fn authentication_result_code(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(code) = number.as_u64() {
                u32::try_from(code).ok()
            } else {
                // A negative value is an HRESULT sent as a signed 32-bit integer.
                number
                    .as_i64()
                    .and_then(|code| i32::try_from(code).ok())
                    .map(|code| u32::from_ne_bytes(code.to_ne_bytes()))
            }
        }
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    }
}
