//! Module containing the core functionality for CSRF protection

use std::error::Error;
use std::{fmt, mem, str};

use crypto::aead::{AeadEncryptor, AeadDecryptor};
use crypto::aes::KeySize;
use crypto::aes_gcm::AesGcm;
use crypto::chacha20poly1305::ChaCha20Poly1305;
use crypto::hmac::Hmac;
use crypto::mac::{Mac, MacResult};
use crypto::scrypt::{scrypt, ScryptParams};
use crypto::sha2::Sha256;
use data_encoding::{BASE64, BASE64URL};
use ring::rand::{SystemRandom, SecureRandom};
use time;
#[cfg(feature = "iron")]
use typemap;


/// The name of the cookie for the CSRF validation data and signature.
pub const CSRF_COOKIE_NAME: &'static str = "csrf";

/// The name of the form field for the CSRF token.
pub const CSRF_FORM_FIELD: &'static str = "csrf-token";

/// The name of the HTTP header for the CSRF token.
pub const CSRF_HEADER: &'static str = "X-CSRF-Token";

/// The name of the query parameter for the CSRF token.
pub const CSRF_QUERY_STRING: &'static str = "csrf-token";

const SCRYPT_SALT: &'static [u8; 21] = b"rust-csrf-scrypt-salt";


/// An `enum` of all CSRF related errors.
#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum CsrfError {
    /// There was an internal error.
    InternalError,
    /// There was CSRF token validation failure.
    ValidationFailure,
}

impl Error for CsrfError {
    fn description(&self) -> &str {
        match *self {
            CsrfError::InternalError => "CSRF library error",
            CsrfError::ValidationFailure => "CSRF validation failed",
        }
    }
}

impl fmt::Display for CsrfError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}


/// A signed, encrypted CSRF token that is suitable to be displayed to end users.
#[derive(Eq, PartialEq, Debug, Clone, Hash)]
pub struct CsrfToken {
    bytes: Vec<u8>,
}

impl CsrfToken {
    /// Create a new token from the given bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        // TODO make this return a Result and check that bytes is long enough
        CsrfToken { bytes: bytes }
    }

    /// Retrieve the CSRF token as a base64 encoded string.
    pub fn b64_string(&self) -> String {
        BASE64.encode(&self.bytes)
    }

    /// Retrieve the CSRF token as a URL safe base64 encoded string.
    pub fn b64_url_string(&self) -> String {
        BASE64URL.encode(&self.bytes)
    }

    /// Get be raw value of this token.
    pub fn value(&self) -> &[u8] {
        &self.bytes
    }
}


/// A signed, encrypted CSRF cookie that is suitable to be displayed to end users.
#[derive(Debug, Eq, PartialEq, Clone, Hash)]
pub struct CsrfCookie {
    bytes: Vec<u8>,
}

impl CsrfCookie {
    /// Create a new cookie from hte given token bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        // TODO make this return a Result and check that bytes is long enough
        CsrfCookie { bytes: bytes }
    }

    /// Get the base64 value of this cookie.
    pub fn b64_string(&self) -> String {
        BASE64.encode(&self.bytes)
    }

    /// Get be raw value of this cookie.
    pub fn value(&self) -> &[u8] {
        &self.bytes
    }
}


/// Internal represenation of an unencrypted CSRF token. This is not suitable to send to end users.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnencryptedCsrfToken {
    token: Vec<u8>,
}

impl UnencryptedCsrfToken {
    /// Create a new unenrypted token.
    pub fn new(token: Vec<u8>) -> Self {
        UnencryptedCsrfToken { token: token }
    }

    /// Retrieve the token value as bytes.
    #[deprecated]
    pub fn token(&self) -> &[u8] {
        &self.token
    }

    /// Retrieve the token value as bytes.
    pub fn value(&self) -> &[u8] {
        &self.token
    }
}


/// Internal represenation of an unencrypted CSRF cookie. This is not suitable to send to end users.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnencryptedCsrfCookie {
    expires: i64,
    token: Vec<u8>,
}

impl UnencryptedCsrfCookie {
    /// Create a new unenrypted cookie.
    pub fn new(expires: i64, token: Vec<u8>) -> Self {
        UnencryptedCsrfCookie {
            expires: expires,
            token: token,
        }
    }

    /// Retrieve the token value as bytes.
    pub fn value(&self) -> &[u8] {
        &self.token
    }
}

/// The base trait that allows a developer to add CSRF protection to an application.
pub trait CsrfProtection: Send + Sync {
    /// Use a key derivation function (KDF) to generate key material.
    ///
    /// # Panics
    /// This function may panic if the underlying crypto library fails catastrophically.
    fn from_password(password: &[u8]) -> Self;

    /// Given a nonce and a time to live (TTL), create a cookie to send to the end user.
    fn generate_cookie(&self, token_value: &[u8; 64], ttl_seconds: i64) -> Result<CsrfCookie, CsrfError>;

    /// Given a nonce, create a token to send to the end user.
    fn generate_token(&self, token_value: &[u8; 64]) -> Result<CsrfToken, CsrfError>;

    /// Given a decoded byte array, deserialize, decrypt, and verify the cookie.
    fn parse_cookie(&self, cookie: &[u8]) -> Result<UnencryptedCsrfCookie, CsrfError>;

    /// Given a decoded byte array, deserialize, decrypt, and verify the token.
    fn parse_token(&self, token: &[u8]) -> Result<UnencryptedCsrfToken, CsrfError>;

    /// Provide a random number generator for other functions.
    fn rng(&self) -> &SystemRandom;

    /// Given a token pair that has been parsed, decoded, decrypted, and verified, return whether
    /// or not the token matches the cookie and they have not expired.
    fn verify_token_pair(&self,
                         token: &UnencryptedCsrfToken,
                         cookie: &UnencryptedCsrfCookie)
                         -> bool {
        let tokens_match = token.token == cookie.token;
        if !tokens_match {
            debug!("Token did not match cookie: T: {:?}, C: {:?}", BASE64.encode(&token.token), BASE64.encode(&cookie.token));
        }

        let now = time::precise_time_s() as i64;
        let not_expired = cookie.expires > now;
        if !not_expired {
            debug!("Cookie expired. Expiration: {}, Current time: {}", cookie.expires, now);
        }

        tokens_match && not_expired
    }

    /// Given a buffer, fill it with random bytes or error if this is not possible.
    fn random_bytes(&self, buf: &mut [u8]) -> Result<(), CsrfError> {
        self.rng()
            .fill(buf)
            .map_err(|_| {
                warn!("Failed to get random bytes");
                CsrfError::InternalError
            })
    }

    /// Given an optional previous token and a TTL, generate a matching token and cookie pair.
    fn generate_token_pair(&self,
                           previous_token_value: Option<&[u8; 64]>,
                           ttl_seconds: i64)
                           -> Result<(CsrfToken, CsrfCookie), CsrfError> {
        let token = match previous_token_value {
            Some(ref previous) => *previous.clone(),
            None => {
                debug!("Generating new CSRF token.");
                let mut token = [0; 64];
                self.random_bytes(&mut token)?;
                token
            },
        };

        match (self.generate_token(&token), self.generate_cookie(&token, ttl_seconds)) {
            (Ok(t), Ok(c)) => Ok((t, c)),
            _ => Err(CsrfError::ValidationFailure),
        }
    }
}


/// Uses HMAC to provide authenticated CSRF tokens and cookies.
pub struct HmacCsrfProtection {
    rng: SystemRandom,
    hmac_key: [u8; 32],
}

impl HmacCsrfProtection {
    /// Given an HMAC key, return an `HmacCsrfProtection` instance.
    pub fn from_key(hmac_key: [u8; 32]) -> Self {
        HmacCsrfProtection {
            rng: SystemRandom::new(),
            hmac_key: hmac_key,
        }
    }

    fn hmac(&self) -> Hmac<Sha256> {
        Hmac::new(Sha256::new(), &self.hmac_key)
    }
}

impl CsrfProtection for HmacCsrfProtection {
    /// Using `scrypt` with params `n=12`, `r=8`, `p=1`, generate the key material used for the
    /// underlying crypto functions.
    ///
    /// # Panics
    /// This function may panic if the underlying crypto library fails catastrophically.
    fn from_password(password: &[u8]) -> Self {
        let params = if cfg!(test) {
            // scrypt is *slow*, so use these params for testing
            ScryptParams::new(1, 8, 1)
        } else {
            ScryptParams::new(12, 8, 1)
        };

        let mut aead_key = [0; 32];
        info!("Generating key material. This may take some time.");
        scrypt(password, SCRYPT_SALT, &params, &mut aead_key);
        info!("Key material generated.");

        HmacCsrfProtection::from_key(aead_key)
    }

    fn rng(&self) -> &SystemRandom {
        &self.rng
    }

    fn generate_cookie(&self, token_value: &[u8; 64], ttl_seconds: i64) -> Result<CsrfCookie, CsrfError> {
        let expires = time::precise_time_s() as i64 + ttl_seconds;
        let expires_bytes = unsafe { mem::transmute::<i64, [u8; 8]>(expires) };

        let mut hmac = self.hmac();
        hmac.input(token_value);
        hmac.input(&expires_bytes);
        let mac = hmac.result();
        let code = mac.code();

        let mut transport = [0; 104];

        for i in 0..64 {
            transport[i] = token_value[i];
        }
        for i in 0..8 {
            transport[i + 64] = expires_bytes[i];
        }
        for i in 0..32 {
            transport[i + 72] = code[i];
        }

        Ok(CsrfCookie::new(transport.to_vec()))
    }

    fn generate_token(&self, token_value: &[u8; 64]) -> Result<CsrfToken, CsrfError> {
        let mut hmac = self.hmac();
        hmac.input(token_value);
        let mac = hmac.result();
        let code = mac.code();

        let mut transport = [0; 96];

        for i in 0..64 {
            transport[i] = token_value[i];
        }
        for i in 0..32 {
            transport[i + 64] = code[i];
        }

        Ok(CsrfToken::new(transport.to_vec()))
    }

    fn parse_cookie(&self, cookie: &[u8]) -> Result<UnencryptedCsrfCookie, CsrfError> {
        if cookie.len() != 104 {
            debug!("Cookie too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut cookie_bytes = [0; 64];
        let mut expires_bytes = [0; 8];
        let mut code = [0; 32];

        for i in 0..64 {
            cookie_bytes[i] = cookie[i];
        }
        for i in 0..8 {
            expires_bytes[i] = cookie[i + 64]
        }
        for i in 0..32 {
            code[i] = cookie[i + 72];
        }

        let mac = MacResult::new(&code);
        let mut hmac = self.hmac();
        hmac.input(&cookie_bytes);
        hmac.input(&expires_bytes);
        let result = hmac.result();

        if result != mac {
            info!("CSRF cookie had bad MAC");
            return Err(CsrfError::ValidationFailure);
        }

        let expires = unsafe { mem::transmute::<[u8; 8], i64>(expires_bytes) };

        Ok(UnencryptedCsrfCookie::new(expires, cookie_bytes.to_vec()))
    }

    fn parse_token(&self, token: &[u8]) -> Result<UnencryptedCsrfToken, CsrfError> {
        if token.len() != 96 {
            debug!("Token too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut token_bytes = [0; 64];
        let mut code = [0; 32];

        for i in 0..64 {
            token_bytes[i] = token[i];
        }
        for i in 0..32 {
            code[i] = token[i + 64];
        }

        let mac = MacResult::new(&code);
        let mut hmac = self.hmac();
        hmac.input(&token_bytes);
        let result = hmac.result();

        if result != mac {
            info!("CSRF token had bad MAC");
            return Err(CsrfError::ValidationFailure);
        }

        Ok(UnencryptedCsrfToken::new(token_bytes.to_vec()))
    }
}


/// Uses AES-GCM to provide signed, encrypted CSRF tokens and cookies.
pub struct AesGcmCsrfProtection {
    rng: SystemRandom,
    aead_key: [u8; 32],
}

impl AesGcmCsrfProtection {
    /// Given an AES256 key, return an `AesGcmCsrfProtection` instance.
    pub fn from_key(aead_key: [u8; 32]) -> Self {
        AesGcmCsrfProtection {
            rng: SystemRandom::new(),
            aead_key: aead_key,
        }
    }

    fn aead<'a>(&self, nonce: &[u8; 12]) -> AesGcm<'a> {
        AesGcm::new(KeySize::KeySize256, &self.aead_key, nonce, &[])
    }
}

impl CsrfProtection for AesGcmCsrfProtection {
    /// Using `scrypt` with params `n=12`, `r=8`, `p=1`, generate the key material used for the
    /// underlying crypto functions.
    ///
    /// # Panics
    /// This function may panic if the underlying crypto library fails catastrophically.
    fn from_password(password: &[u8]) -> Self {
        let params = if cfg!(test) {
            // scrypt is *slow*, so use these params for testing
            ScryptParams::new(1, 8, 1)
        } else {
            ScryptParams::new(12, 8, 1)
        };

        let mut aead_key = [0; 32];
        info!("Generating key material. This may take some time.");
        scrypt(password, SCRYPT_SALT, &params, &mut aead_key);
        info!("Key material generated.");

        AesGcmCsrfProtection::from_key(aead_key)
    }

    fn rng(&self) -> &SystemRandom {
        &self.rng
    }

    fn generate_cookie(&self, token_value: &[u8; 64], ttl_seconds: i64) -> Result<CsrfCookie, CsrfError> {
        let expires = time::precise_time_s() as i64 + ttl_seconds;
        let expires_bytes = unsafe { mem::transmute::<i64, [u8; 8]>(expires) };

        let mut nonce = [0; 12];
        self.random_bytes(&mut nonce)?;

        let mut padding = [0; 16];
        self.random_bytes(&mut padding)?;

        let mut plaintext = [0; 88];

        for i in 0..16 {
            plaintext[i] = padding[i];
        }
        for i in 0..8 {
            plaintext[i + 16] = expires_bytes[i];
        }
        for i in 0..64 {
            plaintext[i + 24] = token_value[i];
        }

        let mut ciphertext = [0; 88];
        let mut tag = [0; 16];
        let mut aead = self.aead(&nonce);

        aead.encrypt(&plaintext, &mut ciphertext, &mut tag);

        let mut transport = [0; 116];

        for i in 0..88 {
            transport[i] = ciphertext[i];
        }
        for i in 0..12 {
            transport[i + 88] = nonce[i];
        }
        for i in 0..16 {
            transport[i + 100] = tag[i];
        }

        Ok(CsrfCookie::new(transport.to_vec()))
    }

    fn generate_token(&self, token_value: &[u8; 64]) -> Result<CsrfToken, CsrfError> {
        let mut nonce = [0; 12];
        self.random_bytes(&mut nonce)?;

        let mut padding = [0; 16];
        self.random_bytes(&mut padding)?;

        let mut plaintext = [0; 80];

        for i in 0..16 {
            plaintext[i] = padding[i];
        }
        for i in 0..64 {
            plaintext[i + 16] = token_value[i];
        }

        let mut ciphertext = [0; 80];
        let mut tag = vec![0; 16];
        let mut aead = self.aead(&nonce);

        aead.encrypt(&plaintext, &mut ciphertext, &mut tag);

        let mut transport = [0; 108];

        for i in 0..80 {
            transport[i] = ciphertext[i];
        }
        for i in 0..12 {
            transport[i + 80] = nonce[i];
        }
        for i in 0..16 {
            transport[i + 92] = tag[i];
        }

        Ok(CsrfToken::new(transport.to_vec()))
    }

    fn parse_cookie(&self, cookie: &[u8]) -> Result<UnencryptedCsrfCookie, CsrfError> {
        if cookie.len() != 116 {
            debug!("Cookie too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut ciphertext = [0; 88];
        let mut nonce = [0; 12];
        let mut tag = [0; 16];

        for i in 0..88 {
            ciphertext[i] = cookie[i];
        }
        for i in 0..12 {
            nonce[i] = cookie[i + 88];
        }
        for i in 0..16 {
            tag[i] = cookie[i + 100];
        }

        let mut plaintext = [0; 88];
        let mut aead = self.aead(&nonce);
        if !aead.decrypt(&ciphertext, &mut plaintext, &tag) {
            info!("Failed to decrypt CSRF cookie");
            return Err(CsrfError::ValidationFailure);
        }

        let mut expires_bytes = [0; 8];
        let mut token = [0; 64];

        // skip 16 bytes of padding
        for i in 0..8 {
            expires_bytes[i] = plaintext[i + 16];
        }
        for i in 0..64 {
            token[i] = plaintext[i + 24];
        }

        let expires = unsafe { mem::transmute::<[u8; 8], i64>(expires_bytes) };

        Ok(UnencryptedCsrfCookie::new(expires, token.to_vec()))
    }

    fn parse_token(&self, token: &[u8]) -> Result<UnencryptedCsrfToken, CsrfError> {
        if token.len() != 108 {
            debug!("Token too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut ciphertext = [0; 80];
        let mut nonce = [0; 12];
        let mut tag = [0; 16];

        for i in 0..80 {
            ciphertext[i] = token[i];
        }
        for i in 0..12 {
            nonce[i] = token[i + 80];
        }
        for i in 0..16 {
            tag[i] = token[i + 92];
        }

        let mut plaintext = [0; 80];
        let mut aead = self.aead(&nonce);
        if !aead.decrypt(&ciphertext, &mut plaintext, &tag) {
            info!("Failed to decrypt CSRF token");
            return Err(CsrfError::ValidationFailure);
        }

        let mut token = [0; 64];

        // skip 16 bytes of padding
        for i in 0..64 {
            token[i] = plaintext[i + 16];
        }

        Ok(UnencryptedCsrfToken::new(token.to_vec()))
    }
}


/// Uses ChaCha20Poly1305 to provide signed, encrypted CSRF tokens and cookies.
pub struct ChaCha20Poly1305CsrfProtection {
    rng: SystemRandom,
    aead_key: [u8; 32],
}

impl ChaCha20Poly1305CsrfProtection {
    /// Given a key, return a `ChaCha20Poly1305CsrfProtection` instance.
    pub fn from_key(aead_key: [u8; 32]) -> Self {
        ChaCha20Poly1305CsrfProtection {
            rng: SystemRandom::new(),
            aead_key: aead_key,
        }
    }

    fn aead(&self, nonce: &[u8; 8]) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.aead_key, nonce, &[])
    }
}

impl CsrfProtection for ChaCha20Poly1305CsrfProtection {
    /// Using `scrypt` with params `n=12`, `r=8`, `p=1`, generate the key material used for the
    /// underlying crypto functions.
    ///
    /// # Panics
    /// This function may panic if the underlying crypto library fails catastrophically.
    fn from_password(password: &[u8]) -> Self {
        let params = if cfg!(test) {
            // scrypt is *slow*, so use these params for testing
            ScryptParams::new(1, 8, 1)
        } else {
            ScryptParams::new(12, 8, 1)
        };

        let mut aead_key = [0; 32];
        info!("Generating key material. This may take some time.");
        scrypt(password, SCRYPT_SALT, &params, &mut aead_key);
        info!("Key material generated.");

        ChaCha20Poly1305CsrfProtection::from_key(aead_key)
    }

    fn rng(&self) -> &SystemRandom {
        &self.rng
    }

    fn generate_cookie(&self, token_value: &[u8; 64], ttl_seconds: i64) -> Result<CsrfCookie, CsrfError> {
        let expires = time::precise_time_s() as i64 + ttl_seconds;
        let expires_bytes = unsafe { mem::transmute::<i64, [u8; 8]>(expires) };

        let mut nonce = [0; 8];
        self.random_bytes(&mut nonce)?;

        let mut padding = [0; 16];
        self.random_bytes(&mut padding)?;

        let mut plaintext = [0; 88];

        for i in 0..16 {
            plaintext[i] = padding[i];
        }
        for i in 0..8 {
            plaintext[i + 16] = expires_bytes[i];
        }
        for i in 0..64 {
            plaintext[i + 24] = token_value[i];
        }

        let mut ciphertext = [0; 88];
        let mut tag = [0; 16];
        let mut aead = self.aead(&nonce);

        aead.encrypt(&plaintext, &mut ciphertext, &mut tag);

        let mut transport = [0; 112];

        for i in 0..88 {
            transport[i] = ciphertext[i];
        }
        for i in 0..8 {
            transport[i + 88] = nonce[i];
        }
        for i in 0..16 {
            transport[i + 96] = tag[i];
        }

        Ok(CsrfCookie::new(transport.to_vec()))
    }

    fn generate_token(&self, token_value: &[u8; 64]) -> Result<CsrfToken, CsrfError> {
        let mut nonce = [0; 8];
        self.random_bytes(&mut nonce)?;

        let mut padding = [0; 16];
        self.random_bytes(&mut padding)?;

        let mut plaintext = [0; 80];

        for i in 0..16 {
            plaintext[i] = padding[i];
        }
        for i in 0..64 {
            plaintext[i + 16] = token_value[i];
        }

        let mut ciphertext = [0; 80];
        let mut tag = vec![0; 16];
        let mut aead = self.aead(&nonce);

        aead.encrypt(&plaintext, &mut ciphertext, &mut tag);

        let mut transport = [0; 104];

        for i in 0..80 {
            transport[i] = ciphertext[i];
        }
        for i in 0..8 {
            transport[i + 80] = nonce[i];
        }
        for i in 0..16 {
            transport[i + 88] = tag[i];
        }

        Ok(CsrfToken::new(transport.to_vec()))
    }

    fn parse_cookie(&self, cookie: &[u8]) -> Result<UnencryptedCsrfCookie, CsrfError> {
        if cookie.len() != 112 {
            debug!("Cookie too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut ciphertext = [0; 88];
        let mut nonce = [0; 8];
        let mut tag = [0; 16];

        for i in 0..88 {
            ciphertext[i] = cookie[i];
        }
        for i in 0..8 {
            nonce[i] = cookie[i + 88];
        }
        for i in 0..16 {
            tag[i] = cookie[i + 96];
        }

        let mut plaintext = [0; 88];
        let mut aead = self.aead(&nonce);
        if !aead.decrypt(&ciphertext, &mut plaintext, &tag) {
            info!("Failed to decrypt CSRF cookie");
            return Err(CsrfError::ValidationFailure);
        }

        let mut expires_bytes = [0; 8];
        let mut token = [0; 64];

        // skip 16 bytes of padding
        for i in 0..8 {
            expires_bytes[i] = plaintext[i + 16];
        }
        for i in 0..64 {
            token[i] = plaintext[i + 24];
        }

        let expires = unsafe { mem::transmute::<[u8; 8], i64>(expires_bytes) };

        Ok(UnencryptedCsrfCookie::new(expires, token.to_vec()))
    }

    fn parse_token(&self, token: &[u8]) -> Result<UnencryptedCsrfToken, CsrfError> {
        if token.len() != 104 {
            debug!("Token too small. Not parsed.");
            return Err(CsrfError::ValidationFailure);
        }

        let mut ciphertext = [0; 80];
        let mut nonce = [0; 8];
        let mut tag = [0; 16];

        for i in 0..80 {
            ciphertext[i] = token[i];
        }
        for i in 0..8 {
            nonce[i] = token[i + 80];
        }
        for i in 0..16 {
            tag[i] = token[i + 88];
        }

        let mut plaintext = [0; 80];
        let mut aead = self.aead(&nonce);
        if !aead.decrypt(&ciphertext, &mut plaintext, &tag) {
            info!("Failed to decrypt CSRF token");
            return Err(CsrfError::ValidationFailure);
        }

        let mut token = [0; 64];

        // skip 16 bytes of padding
        for i in 0..64 {
            token[i] = plaintext[i + 16];
        }

        Ok(UnencryptedCsrfToken::new(token.to_vec()))
    }
}


#[cfg(feature = "iron")]
impl typemap::Key for CsrfToken {
    type Value = CsrfToken;
}


#[cfg(test)]
mod tests {
    // TODO write test that ensures encrypted messages don't contain the plaintext
    // TODO test that checks tokens are repeated when given Some
    // TODO use macros for writing all of these

    macro_rules! test_cases {
        ($strct: ident, $md: ident) => {
            mod $md {
                use $crate::core::{CsrfProtection, $strct};
                use data_encoding::BASE64;

                const KEY_32: [u8; 32] = *b"01234567012345670123456701234567";

                #[test]
                fn from_password() {
                    let _ = $strct::from_password(b"correct horse battery staple");
                }

                #[test]
                fn verification_succeeds() {
                    let protect = $strct::from_key(KEY_32);
                    let (token, cookie) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/cookie pair");
                    let ref token = BASE64.decode(token.b64_string().as_bytes()).expect("token not base64");
                    let token = protect.parse_token(&token).expect("token not parsed");
                    let ref cookie = BASE64.decode(cookie.b64_string().as_bytes()).expect("cookie not base64");
                    let cookie = protect.parse_cookie(&cookie).expect("cookie not parsed");
                    assert!(protect.verify_token_pair(&token, &cookie),
                            "could not verify token/cookie pair");
                }

                #[test]
                fn modified_cookie_sig_fails() {
                    let protect = $strct::from_key(KEY_32);
                    let (_, mut cookie) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/cookie pair");
                    let cookie_len = cookie.bytes.len();
                    cookie.bytes[cookie_len - 1] ^= 0x01;
                    let ref cookie = BASE64.decode(cookie.b64_string().as_bytes()).expect("cookie not base64");
                    assert!(protect.parse_cookie(&cookie).is_err());
                }

                #[test]
                fn modified_cookie_value_fails() {
                    let protect = $strct::from_key(KEY_32);
                    let (_, mut cookie) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/cookie pair");
                    cookie.bytes[0] ^= 0x01;
                    let ref cookie = BASE64.decode(cookie.b64_string().as_bytes()).expect("cookie not base64");
                    assert!(protect.parse_cookie(&cookie).is_err());
                }

                #[test]
                fn modified_token_sig_fails() {
                    let protect = $strct::from_key(KEY_32);
                    let (mut token, _) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/token pair");
                    let token_len = token.bytes.len();
                    token.bytes[token_len - 1] ^= 0x01;
                    let ref token = BASE64.decode(token.b64_string().as_bytes()).expect("token not base64");
                    assert!(protect.parse_token(&token).is_err());
                }

                #[test]
                fn modified_token_value_fails() {
                    let protect = $strct::from_key(KEY_32);
                    let (mut token, _) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/token pair");
                    token.bytes[0] ^= 0x01;
                    let ref token = BASE64.decode(token.b64_string().as_bytes()).expect("token not base64");
                    assert!(protect.parse_token(&token).is_err());
                }

                #[test]
                fn mismatched_cookie_token_fail() {
                    let protect = $strct::from_key(KEY_32);
                    let (token, _) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/token pair");
                    let (_, cookie) = protect.generate_token_pair(None, 300)
                        .expect("couldn't generate token/token pair");

                    let ref token = BASE64.decode(token.b64_string().as_bytes()).expect("token not base64");
                    let token = protect.parse_token(&token).expect("token not parsed");
                    let ref cookie = BASE64.decode(cookie.b64_string().as_bytes()).expect("cookie not base64");
                    let cookie = protect.parse_cookie(&cookie).expect("cookie not parsed");
                    assert!(!protect.verify_token_pair(&token, &cookie),
                            "verified token/cookie pair when failure expected");
                }

                #[test]
                fn expired_token_fail() {
                    let protect = $strct::from_key(KEY_32);
                    let (token, cookie) = protect.generate_token_pair(None, -1)
                        .expect("couldn't generate token/cookie pair");
                    let ref token = BASE64.decode(token.b64_string().as_bytes()).expect("token not base64");
                    let token = protect.parse_token(&token).expect("token not parsed");
                    let ref cookie = BASE64.decode(cookie.b64_string().as_bytes()).expect("cookie not base64");
                    let cookie = protect.parse_cookie(&cookie).expect("cookie not parsed");
                    assert!(!protect.verify_token_pair(&token, &cookie),
                            "verified token/cookie pair when failure expected");
                }
            }
        }
    }

    test_cases!(AesGcmCsrfProtection, aesgcm);
    test_cases!(ChaCha20Poly1305CsrfProtection, chacha20poly1305);
    test_cases!(HmacCsrfProtection, hmac);
}
