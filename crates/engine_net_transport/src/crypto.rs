//! AES-256-GCM and random bytes from Windows CNG (bcrypt.dll).
//!
//! Chosen by measurement against RustCrypto `chacha20poly1305` and `ring`
//! (`bench/net-crypto`, ADR 0070): 0.11–0.25 µs per packet with no new dependency, AES-NI
//! underneath, constant time, maintained and FIPS-validated by Microsoft.

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_AES_GCM_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO, BCRYPT_KEY_HANDLE,
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptDecrypt, BCryptDestroyKey, BCryptEncrypt,
    BCryptGenRandom, BCryptGenerateSymmetricKey,
};

/// Bytes in a key.
pub const KEY_BYTES: usize = 32;
/// Bytes in an authentication tag.
pub const TAG_BYTES: usize = 16;
/// Bytes in a nonce.
pub const NONCE_BYTES: usize = 12;

/// A 256-bit secret key. Zeroed when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Key(pub [u8; KEY_BYTES]);

impl Key {
    /// A fresh random key from the system's random number generator.
    pub fn random() -> Self {
        let mut key = [0u8; KEY_BYTES];
        random_bytes(&mut key);
        Self(key)
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: a valid, aligned pointer to a byte we own; volatile so the
            // compiler cannot drop the wipe of memory that is about to be freed.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// Fills `out` from the system's preferred random number generator.
pub fn random_bytes(out: &mut [u8]) {
    for chunk in out.chunks_mut(u32::MAX as usize) {
        // SAFETY: the buffer is valid for `chunk.len()` bytes; a null algorithm handle with
        // BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented form.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        // The system RNG does not fail on supported Windows versions; never hand out
        // predictable bytes if it somehow does.
        assert!(status == 0, "BCryptGenRandom failed: {status:#x}");
    }
}

/// A random `u64`.
pub fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    random_bytes(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// The nonce of the `sequence`-th message under one key: 32 zero bits, then the sequence
/// little-endian (the form netcode uses). Unique as long as a key never reuses a sequence.
pub fn sequence_nonce(sequence: u64) -> [u8; NONCE_BYTES] {
    let mut nonce = [0u8; NONCE_BYTES];
    nonce[4..].copy_from_slice(&sequence.to_le_bytes());
    nonce
}

/// Encryption or decryption failed (a tampered, truncated or foreign message).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("authentication failed")]
pub struct AuthError;

/// AES-256-GCM under one key.
pub struct Cipher {
    key: BCRYPT_KEY_HANDLE,
}

// SAFETY: a CNG key handle can be used from any thread; `Cipher` is never shared between
// threads without `&mut` (it is not `Sync`), so calls on one handle never overlap.
unsafe impl Send for Cipher {}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(AES-256-GCM)")
    }
}

impl Cipher {
    /// A cipher for `key`.
    pub fn new(key: &Key) -> Self {
        let mut handle: BCRYPT_KEY_HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle is valid on Windows 10+; CNG allocates the key object
        // itself (null buffer); the secret is valid for 32 bytes during the call.
        let status = unsafe {
            BCryptGenerateSymmetricKey(
                BCRYPT_AES_GCM_ALG_HANDLE,
                &mut handle,
                std::ptr::null_mut(),
                0,
                key.0.as_ptr(),
                KEY_BYTES as u32,
                0,
            )
        };
        // Only fails for invalid arguments or out of memory.
        assert!(
            status == 0,
            "BCryptGenerateSymmetricKey failed: {status:#x}"
        );
        Self { key: handle }
    }

    fn mode_info(
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        tag: *mut u8,
    ) -> BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: 1,
            // CNG takes mutable pointers but only reads the nonce and the AAD.
            pbNonce: nonce.as_ptr().cast_mut(),
            cbNonce: NONCE_BYTES as u32,
            pbAuthData: aad.as_ptr().cast_mut(),
            cbAuthData: aad.len() as u32,
            pbTag: tag,
            cbTag: TAG_BYTES as u32,
            pbMacContext: std::ptr::null_mut(),
            cbMacContext: 0,
            cbAAD: 0,
            cbData: 0,
            dwFlags: 0,
        }
    }

    /// Encrypts `buf` in place and returns the tag.
    pub fn seal(&self, nonce: &[u8; NONCE_BYTES], aad: &[u8], buf: &mut [u8]) -> [u8; TAG_BYTES] {
        let mut tag = [0u8; TAG_BYTES];
        let info = Self::mode_info(nonce, aad, tag.as_mut_ptr());
        let mut written = 0u32;
        // SAFETY: in-place encryption (input == output, which CNG supports for symmetric
        // ciphers); every pointer in `info` outlives the call.
        let status = unsafe {
            BCryptEncrypt(
                self.key,
                buf.as_ptr(),
                buf.len() as u32,
                std::ptr::from_ref(&info).cast(),
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut written,
                0,
            )
        };
        assert!(status == 0, "BCryptEncrypt failed: {status:#x}");
        tag
    }

    /// Decrypts `buf` in place if `tag` authenticates it with `aad`. On failure the
    /// buffer's contents are unspecified.
    pub fn open(
        &self,
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG_BYTES],
    ) -> Result<(), AuthError> {
        let mut tag = *tag;
        let info = Self::mode_info(nonce, aad, tag.as_mut_ptr());
        let mut written = 0u32;
        // SAFETY: as in `seal`.
        let status = unsafe {
            BCryptDecrypt(
                self.key,
                buf.as_ptr(),
                buf.len() as u32,
                std::ptr::from_ref(&info).cast(),
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut written,
                0,
            )
        };
        if status == 0 { Ok(()) } else { Err(AuthError) }
    }

    /// Seals `buf[..len]` and appends the tag at `buf[len..len + 16]`.
    pub fn seal_append(
        &self,
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        buf: &mut [u8],
        len: usize,
    ) -> usize {
        let tag = self.seal(nonce, aad, &mut buf[..len]);
        buf[len..len + TAG_BYTES].copy_from_slice(&tag);
        len + TAG_BYTES
    }

    /// Opens a message with its tag appended; returns the plaintext length.
    pub fn open_appended(
        &self,
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        buf: &mut [u8],
    ) -> Result<usize, AuthError> {
        let len = buf.len().checked_sub(TAG_BYTES).ok_or(AuthError)?;
        let (data, tag) = buf.split_at_mut(len);
        let tag: &[u8; TAG_BYTES] = (&*tag).try_into().map_err(|_| AuthError)?;
        self.open(nonce, aad, data, tag)?;
        Ok(len)
    }
}

impl Drop for Cipher {
    fn drop(&mut self) {
        // SAFETY: the handle came from BCryptGenerateSymmetricKey and is destroyed once.
        unsafe { BCryptDestroyKey(self.key) };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// NIST GCM test vector (gcmEncryptExtIV256.rsp style; McGrew & Viega test case 16).
    #[test]
    fn aes_256_gcm_matches_the_reference_vector() {
        let key = Key(
            hex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
                .try_into()
                .unwrap(),
        );
        let nonce: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let mut text = hex(concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39"
        ));
        let cipher = Cipher::new(&key);
        let tag = cipher.seal(&nonce, &aad, &mut text);
        assert_eq!(
            text,
            hex(concat!(
                "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa",
                "8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662"
            ))
        );
        assert_eq!(tag.to_vec(), hex("76fc6ece0f4e1768cddf8853bb2d551b"));
        cipher.open(&nonce, &aad, &mut text, &tag).unwrap();
        assert_eq!(&text[..4], &[0xd9, 0x31, 0x32, 0x25]);
    }

    #[test]
    fn tampering_is_rejected() {
        let cipher = Cipher::new(&Key::random());
        let nonce = sequence_nonce(7);
        let mut buf = [0u8; 64];
        buf[..5].copy_from_slice(b"hello");
        let len = cipher.seal_append(&nonce, b"aad", &mut buf, 5);
        for i in 0..len {
            let mut copy = buf;
            copy[i] ^= 0x10;
            assert!(
                cipher
                    .open_appended(&nonce, b"aad", &mut copy[..len])
                    .is_err()
            );
        }
        let mut copy = buf;
        assert!(
            cipher
                .open_appended(&sequence_nonce(8), b"aad", &mut copy[..len])
                .is_err()
        );
        let mut copy = buf;
        assert!(
            cipher
                .open_appended(&nonce, b"aae", &mut copy[..len])
                .is_err()
        );
        let mut copy = buf;
        assert_eq!(
            cipher.open_appended(&nonce, b"aad", &mut copy[..len]),
            Ok(5)
        );
        assert_eq!(&copy[..5], b"hello");
    }
}
