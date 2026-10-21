//! Pairing crypto for PC receivers, with Windows CNG (no new crates): P-256 ECDH and SHA-256.
//!
//! Numeric comparison as in Bluetooth: both PCs show a 6-digit code and both users confirm that
//! the codes match. The receiver commits to its nonce before it learns the controlling PC's, so a
//! man in the middle, who cannot choose both nonces after seeing the other's, matches the codes
//! with a chance of one in a million instead of grinding keys until they match.

use windows::Win32::Security::Cryptography::*;

use crate::link::Public;

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut out = [0; 32];
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, &parts.concat(), &mut out) }.ok().expect("SHA-256");
    out
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0; N];
    getrandom::fill(&mut b).expect("getrandom");
    b
}

/// An ephemeral P-256 key pair.
pub struct KeyPair(BCRYPT_KEY_HANDLE);

impl KeyPair {
    pub fn new() -> KeyPair {
        let mut h = BCRYPT_KEY_HANDLE::default();
        unsafe {
            BCryptGenerateKeyPair(BCRYPT_ECDH_P256_ALG_HANDLE, &mut h, 256, 0).ok().expect("P-256 key");
            BCryptFinalizeKeyPair(h, 0).ok().expect("P-256 key");
        }
        KeyPair(h)
    }

    pub fn public(&self) -> Public {
        // BCRYPT_ECCKEY_BLOB (magic, length), then X and Y.
        let (mut b, mut n) = ([0u8; 8 + 64], 0);
        unsafe { BCryptExportKey(self.0, None, BCRYPT_ECCPUBLIC_BLOB, Some(&mut b), &mut n, 0) }.ok().expect("export");
        b[8..].try_into().unwrap()
    }

    /// The shared secret with the holder of `peer`; None if that is not a key on the curve.
    pub fn agree(&self, peer: &Public) -> Option<[u8; 32]> {
        let blob = [&BCRYPT_ECDH_PUBLIC_P256_MAGIC.to_le_bytes()[..], &32u32.to_le_bytes(), peer].concat();
        unsafe {
            let mut pk = BCRYPT_KEY_HANDLE::default();
            BCryptImportKeyPair(BCRYPT_ECDH_P256_ALG_HANDLE, None, BCRYPT_ECCPUBLIC_BLOB, &mut pk, &blob, 0).ok().ok()?;
            let mut secret = BCRYPT_SECRET_HANDLE::default();
            let agreed = BCryptSecretAgreement(self.0, pk, &mut secret, 0).ok();
            let _ = BCryptDestroyKey(pk);
            agreed.ok()?;
            let (mut z, mut n) = ([0u8; 32], 0);
            let derived = BCryptDeriveKey(secret, BCRYPT_KDF_RAW_SECRET, None, Some(&mut z), &mut n, 0).ok();
            let _ = BCryptDestroySecret(secret);
            (derived.is_ok() && n == 32).then_some(z)
        }
    }
}

impl Drop for KeyPair {
    fn drop(&mut self) {
        unsafe { let _ = BCryptDestroyKey(self.0); }
    }
}

/// The receiver's commitment to its nonce `nr`, bound to both public keys.
pub fn commit(receiver: &Public, pc: &Public, nr: &[u8; 16]) -> [u8; 32] {
    sha256(&[b"wadisplay commit", receiver, pc, nr])
}

/// The code both screens show (0..1_000_000) and the paired key, from the shared secret, both
/// public keys and both nonces.
pub fn derive(z: &[u8; 32], pc: &Public, receiver: &Public, nc: &[u8; 16], nr: &[u8; 16]) -> (u32, [u8; 32]) {
    let c = sha256(&[b"wadisplay code", pc, receiver, nc, nr]);
    let code = u32::from_le_bytes(c[..4].try_into().unwrap()) % 1_000_000;
    (code, sha256(&[b"wadisplay key", z, pc, receiver, nc, nr]))
}

/// "123 456".
pub fn code_text(code: u32) -> String {
    format!("{:03} {:03}", code / 1000, code % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_and_a_middleman_does_not() {
        let (pc, rx, mid) = (KeyPair::new(), KeyPair::new(), KeyPair::new());
        let (pp, rp) = (pc.public(), rx.public());
        let (nc, nr): ([u8; 16], [u8; 16]) = (random(), random());
        let z = pc.agree(&rp).unwrap();
        assert_eq!(rx.agree(&pp), Some(z));
        let a = derive(&z, &pp, &rp, &nc, &nr);
        assert_eq!(a, derive(&rx.agree(&pp).unwrap(), &pp, &rp, &nc, &nr));
        assert!(a.0 < 1_000_000);
        // Each side talking to the middleman instead gets another key.
        let b = derive(&pc.agree(&mid.public()).unwrap(), &pp, &mid.public(), &nc, &nr);
        assert_ne!(a.1, b.1);
        assert_eq!(commit(&rp, &pp, &nr), commit(&rp, &pp, &nr));
        assert_ne!(commit(&rp, &pp, &nr), commit(&rp, &pp, &nc), "binds the nonce");
        assert_eq!(pc.agree(&[0; 64]), None, "not on the curve");
        assert_eq!(code_text(7042), "007 042");
    }

    #[test]
    fn sha256_known_answer() {
        assert_eq!(wadisplay_proto::hex(&sha256(&[b"a", b"bc"])),
                   "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
