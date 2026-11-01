//! AES-128 in ECB mode with PKCS#7 padding.
//!
//! The WeChat iLink CDN stores media encrypted with AES-128-ECB and
//! PKCS#7 padding (the `openclaw-weixin` protocol document, "CDN media
//! flow"). That is the only reason this module exists: no other adapter
//! needs a block cipher.
//!
//! The cipher is implemented here instead of pulled from a crate because the
//! offline build has no AES implementation available, and an unverifiable
//! hand-written cipher would be worse than none. To make the implementation
//! checkable, the whole cipher is `const fn` and the two canonical FIPS-197
//! / SP 800-38A known-answer vectors are asserted at COMPILE time: a wrong
//! S-box, key schedule or round function fails the build of this crate
//! instead of silently corrupting uploaded media.
//!
//! ECB is used because the platform specifies it. It is not a general purpose
//! mode and this module is not a general purpose cipher API.

/// Multiply two bytes in GF(2^8) with the AES polynomial `0x11b`.
const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    let mut round = 0;
    while round < 8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let carry = a & 0x80;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
        round += 1;
    }
    product
}

/// The multiplicative inverse of a byte in GF(2^8); `0` maps to `0`.
const fn gf_inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    let mut candidate = 1u16;
    while candidate < 256 {
        if gf_mul(value, candidate as u8) == 1 {
            return candidate as u8;
        }
        candidate += 1;
    }
    0
}

/// Rotate an 8-bit value left by `amount` bits.
const fn rotate_left(value: u8, amount: u32) -> u8 {
    (value << amount) | (value >> (8 - amount))
}

/// Build the AES S-box from its definition instead of a copied table.
const fn build_sbox() -> [u8; 256] {
    let mut box_values = [0u8; 256];
    let mut index = 0usize;
    while index < 256 {
        let inverse = gf_inverse(index as u8);
        box_values[index] = inverse
            ^ rotate_left(inverse, 1)
            ^ rotate_left(inverse, 2)
            ^ rotate_left(inverse, 3)
            ^ rotate_left(inverse, 4)
            ^ 0x63;
        index += 1;
    }
    box_values
}

/// Build the inverse S-box from the forward S-box.
const fn build_inverse_sbox(forward: &[u8; 256]) -> [u8; 256] {
    let mut inverse = [0u8; 256];
    let mut index = 0usize;
    while index < 256 {
        inverse[forward[index] as usize] = index as u8;
        index += 1;
    }
    inverse
}

const FORWARD_SBOX: [u8; 256] = build_sbox();
const INVERSE_SBOX: [u8; 256] = build_inverse_sbox(&FORWARD_SBOX);

const ROUND_CONSTANTS: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// The expanded 128-bit key: 44 words of four bytes.
type ExpandedKey = [u8; 176];

/// Expand a 16-byte key into the AES-128 round keys.
const fn expand_key(key: &[u8; 16]) -> ExpandedKey {
    let mut expanded = [0u8; 176];
    let mut index = 0usize;
    while index < 16 {
        expanded[index] = key[index];
        index += 1;
    }
    let mut word = 4usize;
    while word < 44 {
        let mut temp = [
            expanded[(word - 1) * 4],
            expanded[(word - 1) * 4 + 1],
            expanded[(word - 1) * 4 + 2],
            expanded[(word - 1) * 4 + 3],
        ];
        if word % 4 == 0 {
            let rotated = [temp[1], temp[2], temp[3], temp[0]];
            let mut substituted = [0u8; 4];
            let mut byte = 0usize;
            while byte < 4 {
                substituted[byte] = FORWARD_SBOX[rotated[byte] as usize];
                byte += 1;
            }
            temp = substituted;
            temp[0] ^= ROUND_CONSTANTS[word / 4 - 1];
        }
        let mut byte = 0usize;
        while byte < 4 {
            expanded[word * 4 + byte] = expanded[(word - 4) * 4 + byte] ^ temp[byte];
            byte += 1;
        }
        word += 1;
    }
    expanded
}

const fn add_round_key(state: &mut [u8; 16], expanded: &ExpandedKey, round: usize) {
    let mut index = 0usize;
    while index < 16 {
        state[index] ^= expanded[round * 16 + index];
        index += 1;
    }
}

const fn sub_bytes(state: &mut [u8; 16]) {
    let mut index = 0usize;
    while index < 16 {
        state[index] = FORWARD_SBOX[state[index] as usize];
        index += 1;
    }
}

const fn inv_sub_bytes(state: &mut [u8; 16]) {
    let mut index = 0usize;
    while index < 16 {
        state[index] = INVERSE_SBOX[state[index] as usize];
        index += 1;
    }
}

const fn shift_rows(state: &mut [u8; 16]) {
    let source = *state;
    let mut row = 0usize;
    while row < 4 {
        let mut column = 0usize;
        while column < 4 {
            state[4 * column + row] = source[4 * ((column + row) % 4) + row];
            column += 1;
        }
        row += 1;
    }
}

const fn inv_shift_rows(state: &mut [u8; 16]) {
    let source = *state;
    let mut row = 0usize;
    while row < 4 {
        let mut column = 0usize;
        while column < 4 {
            state[4 * ((column + row) % 4) + row] = source[4 * column + row];
            column += 1;
        }
        row += 1;
    }
}

const fn mix_columns(state: &mut [u8; 16]) {
    let source = *state;
    let mut column = 0usize;
    while column < 4 {
        let base = 4 * column;
        let a0 = source[base];
        let a1 = source[base + 1];
        let a2 = source[base + 2];
        let a3 = source[base + 3];
        state[base] = gf_mul(a0, 2) ^ gf_mul(a1, 3) ^ a2 ^ a3;
        state[base + 1] = a0 ^ gf_mul(a1, 2) ^ gf_mul(a2, 3) ^ a3;
        state[base + 2] = a0 ^ a1 ^ gf_mul(a2, 2) ^ gf_mul(a3, 3);
        state[base + 3] = gf_mul(a0, 3) ^ a1 ^ a2 ^ gf_mul(a3, 2);
        column += 1;
    }
}

const fn inv_mix_columns(state: &mut [u8; 16]) {
    let source = *state;
    let mut column = 0usize;
    while column < 4 {
        let base = 4 * column;
        let a0 = source[base];
        let a1 = source[base + 1];
        let a2 = source[base + 2];
        let a3 = source[base + 3];
        state[base] = gf_mul(a0, 14) ^ gf_mul(a1, 11) ^ gf_mul(a2, 13) ^ gf_mul(a3, 9);
        state[base + 1] = gf_mul(a0, 9) ^ gf_mul(a1, 14) ^ gf_mul(a2, 11) ^ gf_mul(a3, 13);
        state[base + 2] = gf_mul(a0, 13) ^ gf_mul(a1, 9) ^ gf_mul(a2, 14) ^ gf_mul(a3, 11);
        state[base + 3] = gf_mul(a0, 11) ^ gf_mul(a1, 13) ^ gf_mul(a2, 9) ^ gf_mul(a3, 14);
        column += 1;
    }
}

/// Encrypt one 16-byte block with AES-128.
const fn encrypt_block(block: &[u8; 16], expanded: &ExpandedKey) -> [u8; 16] {
    let mut state = *block;
    add_round_key(&mut state, expanded, 0);
    let mut round = 1usize;
    while round < 10 {
        sub_bytes(&mut state);
        shift_rows(&mut state);
        mix_columns(&mut state);
        add_round_key(&mut state, expanded, round);
        round += 1;
    }
    sub_bytes(&mut state);
    shift_rows(&mut state);
    add_round_key(&mut state, expanded, 10);
    state
}

/// Decrypt one 16-byte block with AES-128.
const fn decrypt_block(block: &[u8; 16], expanded: &ExpandedKey) -> [u8; 16] {
    let mut state = *block;
    add_round_key(&mut state, expanded, 10);
    let mut round = 9usize;
    loop {
        inv_shift_rows(&mut state);
        inv_sub_bytes(&mut state);
        add_round_key(&mut state, expanded, round);
        if round == 0 {
            break;
        }
        inv_mix_columns(&mut state);
        round -= 1;
    }
    state
}

/// The compile-time known-answer check.
///
/// `FIPS-197` appendix C.1 and `SP 800-38A` F.1.1 (ECB-AES128.Encrypt).
const fn bytes_equal(left: &[u8; 16], right: &[u8; 16]) -> bool {
    let mut index = 0usize;
    while index < 16 {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const _: () = {
    let key_a: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
        0x0e, 0x0f,
    ];
    let plain_a: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0xff,
    ];
    let cipher_a: [u8; 16] = [
        0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
        0xc5, 0x5a,
    ];
    let key_b: [u8; 16] = [
        0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
        0x4f, 0x3c,
    ];
    let plain_b: [u8; 16] = [
        0x32, 0x43, 0xf6, 0xa8, 0x88, 0x5a, 0x30, 0x8d, 0x31, 0x31, 0x98, 0xa2, 0xe0, 0x37,
        0x07, 0x34,
    ];
    let cipher_b: [u8; 16] = [
        0x39, 0x25, 0x84, 0x1d, 0x02, 0xdc, 0x09, 0xfb, 0xdc, 0x11, 0x85, 0x97, 0x19, 0x6a,
        0x0b, 0x32,
    ];

    let expanded_a = expand_key(&key_a);
    let expanded_b = expand_key(&key_b);

    assert!(bytes_equal(&encrypt_block(&plain_a, &expanded_a), &cipher_a));
    assert!(bytes_equal(&encrypt_block(&plain_b, &expanded_b), &cipher_b));
    assert!(bytes_equal(&decrypt_block(&cipher_a, &expanded_a), &plain_a));
    assert!(bytes_equal(&decrypt_block(&cipher_b, &expanded_b), &plain_b));
};

/// Why a media payload could not be encrypted or decrypted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AesError {
    /// The key was not exactly 16 bytes.
    #[error("aes-128 key must be 16 bytes, got {0}")]
    BadKeyLength(usize),
    /// The ciphertext was not a whole number of blocks.
    #[error("ciphertext length {0} is not a multiple of the 16-byte block size")]
    BadBlockLength(usize),
    /// The PKCS#7 padding was missing or malformed.
    #[error("pkcs7 padding is invalid")]
    BadPadding,
}

/// Encrypt bytes with AES-128-ECB and PKCS#7 padding.
pub fn encrypt_ecb_pkcs7(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, AesError> {
    if key.len() != 16 {
        return Err(AesError::BadKeyLength(key.len()));
    }
    let expanded = expand_key(&<[u8; 16]>::try_from(key).map_err(|_| AesError::BadKeyLength(key.len()))?);
    let padding = 16 - (plaintext.len() % 16);
    let mut padded = Vec::with_capacity(plaintext.len() + padding);
    padded.extend_from_slice(plaintext);
    padded.resize(plaintext.len() + padding, padding as u8);
    let mut ciphertext = Vec::with_capacity(padded.len());
    for chunk in padded.chunks_exact(16) {
        let block: [u8; 16] = chunk.try_into().expect("chunk is exactly one block");
        ciphertext.extend_from_slice(&encrypt_block(&block, &expanded));
    }
    Ok(ciphertext)
}

/// Decrypt AES-128-ECB bytes and strip the PKCS#7 padding.
pub fn decrypt_ecb_pkcs7(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, AesError> {
    if key.len() != 16 {
        return Err(AesError::BadKeyLength(key.len()));
    }
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(AesError::BadBlockLength(ciphertext.len()));
    }
    let expanded = expand_key(&<[u8; 16]>::try_from(key).map_err(|_| AesError::BadKeyLength(key.len()))?);
    let mut plaintext = Vec::with_capacity(ciphertext.len());
    for chunk in ciphertext.chunks_exact(16) {
        let block: [u8; 16] = chunk.try_into().expect("chunk is exactly one block");
        plaintext.extend_from_slice(&decrypt_block(&block, &expanded));
    }
    let padding = *plaintext.last().ok_or(AesError::BadPadding)? as usize;
    if padding == 0 || padding > 16 || padding > plaintext.len() {
        return Err(AesError::BadPadding);
    }
    let start = plaintext.len() - padding;
    if plaintext[start..].iter().any(|byte| *byte as usize != padding) {
        return Err(AesError::BadPadding);
    }
    plaintext.truncate(start);
    Ok(plaintext)
}
