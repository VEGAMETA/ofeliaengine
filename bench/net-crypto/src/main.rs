//! ns per packet for AEAD seal/open of one game packet, three implementations:
//! Windows CNG (AES-GCM, ChaCha20-Poly1305 through the pseudo-handles), RustCrypto
//! `chacha20poly1305` and `ring`. `cargo run --release` (ADR 0070).

use std::hint::black_box;
use std::time::Instant;

use windows_sys::Win32::Security::Cryptography::*;

const AAD: [u8; 17] = [7u8; 17];
const SIZES: [usize; 4] = [32, 128, 512, 1200];
const ITERS: usize = 200_000;

struct Cng {
    key: BCRYPT_KEY_HANDLE,
}

impl Cng {
    fn new(alg: BCRYPT_ALG_HANDLE, key: &[u8]) -> Self {
        let mut handle: BCRYPT_KEY_HANDLE = std::ptr::null_mut();
        // SAFETY: valid pseudo-handle, key slice valid for the call.
        let status = unsafe {
            BCryptGenerateSymmetricKey(
                alg,
                &mut handle,
                std::ptr::null_mut(),
                0,
                key.as_ptr(),
                key.len() as u32,
                0,
            )
        };
        assert_eq!(status, 0, "BCryptGenerateSymmetricKey {status:#x}");
        Self { key: handle }
    }

    fn info(nonce: &[u8; 12], tag: &mut [u8; 16]) -> BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: 1,
            pbNonce: nonce.as_ptr() as *mut u8,
            cbNonce: 12,
            pbAuthData: AAD.as_ptr() as *mut u8,
            cbAuthData: AAD.len() as u32,
            pbTag: tag.as_mut_ptr(),
            cbTag: 16,
            pbMacContext: std::ptr::null_mut(),
            cbMacContext: 0,
            cbAAD: 0,
            cbData: 0,
            dwFlags: 0,
        }
    }

    fn seal(&self, nonce: &[u8; 12], buf: &mut [u8], tag: &mut [u8; 16]) {
        let info = Self::info(nonce, tag);
        let mut out = 0u32;
        // SAFETY: in-place encryption, buffers valid for the call.
        let status = unsafe {
            BCryptEncrypt(
                self.key,
                buf.as_ptr(),
                buf.len() as u32,
                &info as *const _ as *const _,
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out,
                0,
            )
        };
        assert_eq!(status, 0);
    }

    fn open(&self, nonce: &[u8; 12], buf: &mut [u8], tag: &mut [u8; 16]) -> bool {
        let info = Self::info(nonce, tag);
        let mut out = 0u32;
        // SAFETY: in-place decryption, buffers valid for the call.
        let status = unsafe {
            BCryptDecrypt(
                self.key,
                buf.as_ptr(),
                buf.len() as u32,
                &info as *const _ as *const _,
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out,
                0,
            )
        };
        status == 0
    }
}

fn measure(
    name: &str,
    size: usize,
    mut seal: impl FnMut(u64, &mut [u8]),
    mut open: impl FnMut(u64, &mut [u8]) -> bool,
) {
    let mut buf = vec![0x5au8; size];
    let mut packets = vec![vec![0u8; size]; 64];
    for (i, p) in packets.iter_mut().enumerate() {
        p.fill(i as u8);
        seal(i as u64, p);
    }
    for i in 0..1000 {
        seal(1000 + i, &mut buf);
    }
    let start = Instant::now();
    for i in 0..ITERS {
        seal(1000 + i as u64, black_box(&mut buf));
    }
    let seal_ns = start.elapsed().as_nanos() as f64 / ITERS as f64;
    let mut work = vec![0u8; size];
    let start = Instant::now();
    for i in 0..ITERS {
        let k = i % 64;
        work.copy_from_slice(&packets[k]);
        assert!(open(k as u64, black_box(&mut work)));
    }
    let open_ns = start.elapsed().as_nanos() as f64 / ITERS as f64;
    println!(
        "{name:<30} {size:>5} B  seal {seal_ns:>6.0} ns  open {open_ns:>6.0} ns  ({:.2} GB/s seal)",
        size as f64 / seal_ns
    );
}

fn nonce12(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

fn main() {
    let key = [0x42u8; 32];
    for &size in &SIZES {
        for (name, alg, klen) in [
            ("CNG AES-128-GCM", BCRYPT_AES_GCM_ALG_HANDLE, 16),
            ("CNG AES-256-GCM", BCRYPT_AES_GCM_ALG_HANDLE, 32),
            ("CNG ChaCha20-Poly1305", BCRYPT_CHACHA20_POLY1305_ALG_HANDLE, 32),
        ] {
            let cng = Cng::new(alg, &key[..klen]);
            let tags = std::cell::RefCell::new(vec![[0u8; 16]; 64]);
            measure(
                name,
                size,
                |seq, buf| {
                    let mut tag = [0u8; 16];
                    cng.seal(&nonce12(seq), buf, &mut tag);
                    if seq < 64 {
                        tags.borrow_mut()[seq as usize] = tag;
                    }
                },
                |seq, buf| {
                    let mut t = tags.borrow()[seq as usize];
                    cng.open(&nonce12(seq), buf, &mut t)
                },
            );
        }
        {
            use chacha20poly1305::aead::{AeadInOut, KeyInit};
            use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
            let cipher = ChaCha20Poly1305::new_from_slice(&key).unwrap();
            let tags = std::cell::RefCell::new(vec![Tag::default(); 64]);
            measure(
                "RustCrypto ChaCha20-Poly1305",
                size,
                |seq, buf| {
                    let n = nonce12(seq);
                    let tag = cipher
                        .encrypt_inout_detached(&Nonce::from(n), &AAD, buf.into())
                        .unwrap();
                    if seq < 64 {
                        tags.borrow_mut()[seq as usize] = tag;
                    }
                },
                |seq, buf| {
                    let n = nonce12(seq);
                    let tag = tags.borrow()[seq as usize];
                    cipher
                        .decrypt_inout_detached(&Nonce::from(n), &AAD, buf.into(), &tag)
                        .is_ok()
                },
            );
        }
        for (name, alg) in [
            ("ring AES-128-GCM", &ring::aead::AES_128_GCM),
            ("ring AES-256-GCM", &ring::aead::AES_256_GCM),
            ("ring ChaCha20-Poly1305", &ring::aead::CHACHA20_POLY1305),
        ] {
            use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey};
            let k = LessSafeKey::new(UnboundKey::new(alg, &key[..alg.key_len()]).unwrap());
            let tags = std::cell::RefCell::new(vec![[0u8; 16]; 64]);
            measure(
                name,
                size,
                |seq, buf| {
                    let tag = k
                        .seal_in_place_separate_tag(
                            Nonce::assume_unique_for_key(nonce12(seq)),
                            Aad::from(AAD),
                            buf,
                        )
                        .unwrap();
                    if seq < 64 {
                        tags.borrow_mut()[seq as usize].copy_from_slice(tag.as_ref());
                    }
                },
                |seq, buf| {
                    let tag = tags.borrow()[seq as usize];
                    k.open_in_place_separate_tag(
                        Nonce::assume_unique_for_key(nonce12(seq)),
                        Aad::from(AAD),
                        ring::aead::Tag::from(tag),
                        buf,
                        0..,
                    )
                    .is_ok()
                },
            );
        }
        println!();
    }
}
