use base64::Engine as _;
use wasm_bindgen::prelude::wasm_bindgen;

/// Computes the `req_cnf` token request parameter for a proof-of-possession key.
///
/// `pop_key` is a base64-encoded PKCS#8 RSA private key, as exported by the Web Crypto API.
/// The same key is later passed through the `rdsaad_pop_key` extension.
#[wasm_bindgen(js_name = rdsAadPopKeyReqCnf)]
pub fn rdsaad_pop_key_req_cnf(pop_key: &str) -> Result<String, wasm_bindgen::JsError> {
    let pop_key = base64::engine::general_purpose::STANDARD
        .decode(pop_key)
        .map_err(|e| wasm_bindgen::JsError::new(&format!("pop key is not base64: {e}")))?;

    ironrdp::connector::rdsaad::pop_key_req_cnf(&pop_key).map_err(|e| wasm_bindgen::JsError::new(&format!("{e:#}")))
}
