use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use rand::RngCore;

use crate::frame::FrameId;

pub struct FrameCrypto {
    key: [u8; 16],
    iv_mask: [u8; 16],
}

impl FrameCrypto {
    pub fn new(key: [u8; 16], iv_mask: [u8; 16]) -> Self {
        Self { key, iv_mask }
    }

    fn build_nonce(&self, frame_id: FrameId) -> [u8; 16] {
        let mut nonce = [0u8; 16];
        nonce[8..12].copy_from_slice(&frame_id.lower_32().to_be_bytes());
        for (n, m) in nonce.iter_mut().zip(self.iv_mask.iter()) {
            *n ^= m;
        }
        nonce
    }

    pub fn encrypt(&self, frame_id: FrameId, data: &mut [u8]) {
        let nonce = self.build_nonce(frame_id);
        let mut cipher = Ctr128BE::<Aes128>::new(&self.key.into(), &nonce.into());
        cipher.apply_keystream(data);
    }

    #[cfg(test)]
    pub fn decrypt(&self, frame_id: FrameId, data: &mut [u8]) {
        self.encrypt(frame_id, data);
    }

    pub fn generate_key() -> [u8; 16] {
        let mut key = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut key);
        key
    }

    pub fn generate_iv_mask() -> [u8; 16] {
        let mut mask = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut mask);
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let crypto = FrameCrypto::new([0x42; 16], [0x37; 16]);
        let original = b"hello world, this is a test frame payload".to_vec();
        let mut data = original.clone();

        crypto.encrypt(FrameId::first(), &mut data);
        assert_ne!(data, original);

        crypto.decrypt(FrameId::first(), &mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn different_frame_ids_produce_different_ciphertext() {
        let crypto = FrameCrypto::new([0xAA; 16], [0xBB; 16]);
        let mut data0 = b"identical".to_vec();
        let mut data1 = b"identical".to_vec();

        crypto.encrypt(FrameId(0), &mut data0);
        crypto.encrypt(FrameId(1), &mut data1);
        assert_ne!(data0, data1);
    }

    #[test]
    fn different_keys_produce_different_ciphertext() {
        let crypto_a = FrameCrypto::new([0x01; 16], [0x00; 16]);
        let crypto_b = FrameCrypto::new([0x02; 16], [0x00; 16]);
        let mut data_a = b"same plaintext".to_vec();
        let mut data_b = b"same plaintext".to_vec();

        crypto_a.encrypt(FrameId::first(), &mut data_a);
        crypto_b.encrypt(FrameId::first(), &mut data_b);
        assert_ne!(data_a, data_b);
    }

    #[test]
    fn nonce_construction() {
        let crypto = FrameCrypto::new([0; 16], [0; 16]);
        let nonce = crypto.build_nonce(FrameId(0));
        assert_eq!(nonce, [0u8; 16]);

        let nonce = crypto.build_nonce(FrameId(1));
        let mut expected = [0u8; 16];
        expected[8..12].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(nonce, expected);
    }

    #[test]
    fn nonce_xors_with_iv_mask() {
        let mask = [0xFF; 16];
        let crypto = FrameCrypto::new([0; 16], mask);
        let nonce = crypto.build_nonce(FrameId(0));
        assert_eq!(nonce, [0xFF; 16]);
    }

    #[test]
    fn empty_data() {
        let crypto = FrameCrypto::new([0x42; 16], [0x37; 16]);
        let mut data = vec![];
        crypto.encrypt(FrameId::first(), &mut data);
        assert!(data.is_empty());
    }

    #[test]
    fn generated_keys_are_random() {
        let k1 = FrameCrypto::generate_key();
        let k2 = FrameCrypto::generate_key();
        assert_ne!(k1, k2);
    }
}

#[cfg(test)]
mod known_answer {
    use super::*;

    // Independent vector: `openssl enc -aes-128-ctr` with nonce = mask XOR (frame id BE at bytes 8..12),
    // the layout of openscreen's frame_crypto.cc.
    #[test]
    fn matches_openssl_ctr_with_openscreen_nonce() {
        let crypto = FrameCrypto::new(
            *b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
            *b"\xf0\xf1\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xfb\xfc\xfd\xfe\xff",
        );
        let mut data = b"The quick brown fox jumps over the lazy dog.\0".to_vec();
        crypto.encrypt(FrameId(0x1234_5678), &mut data);
        let expected = "1022dabd0e804dd14dda3caa127885e579798883612e7fde294a9aad7d82930c75f67158d6ded4741ca3c2cbac";
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }
}
