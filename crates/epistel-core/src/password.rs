// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! App passwords: made by the server, shown once, stored only as a hash.
//!
//! Users never choose a password. A generated one carries 160 bits from the operating system's
//! random source, so guessing is not a way in; the hash is Argon2id all the same, so that a copy
//! of the metadata store gives nothing away either.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    bytes
}

/// A new app password: 32 characters, lower-case letters and the digits 2 to 7.
pub fn generate() -> String {
    // Base32: 20 bytes are 160 bits are exactly 32 characters of 5 bits.
    let bytes: [u8; 20] = random();
    let mut text = String::with_capacity(32);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    text
}

/// The hash to store for `secret`, in PHC string format.
///
/// Takes tens of milliseconds by design. Call it off the async threads.
pub fn hash(secret: &str) -> String {
    let salt = SaltString::encode_b64(&random::<16>()).expect("sixteen bytes are a valid salt");
    let hash = Argon2::default().hash_password(secret.as_bytes(), &salt).expect("default parameters are valid");
    hash.to_string()
}

/// Whether `secret` is the password `stored_hash` was made from. A hash that cannot be read
/// matches nothing.
///
/// As slow as [`hash`]. Call it off the async threads.
pub fn verify(secret: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default().verify_password(secret.as_bytes(), &parsed).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_long_distinct_and_plain() {
        let first = generate();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| ALPHABET.contains(&byte)));
        assert_ne!(first, generate());
    }

    #[test]
    fn a_hash_verifies_only_its_own_password() {
        let secret = generate();
        let stored = hash(&secret);
        assert!(stored.starts_with("$argon2id$"));
        assert_ne!(stored, hash(&secret), "every hash has its own salt");
        assert!(verify(&secret, &stored));
        assert!(!verify(&generate(), &stored));
        assert!(!verify(&secret, "not a hash"));
        assert!(!verify(&secret, ""));
    }
}
