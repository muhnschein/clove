//! A piece bitfield: MSB-first bits, BEP 3 convention.
//!
//! Shared by the wire codec (the `bitfield` message), the picker
//! (availability and our own progress), and resume (on-disk `have`/
//! `verified`). Trailing bits of the final byte are spare and must stay
//! zero — a peer setting them is a protocol violation the wire layer
//! rejects.

/// A fixed-length bit set over piece indices `0..len`.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitfield {
    bits: Vec<u8>,
    len: u32,
}

impl Bitfield {
    /// A bitfield of `len` pieces, all absent.
    #[must_use]
    pub fn empty(len: u32) -> Self {
        Bitfield {
            bits: vec![0u8; byte_len(len)],
            len,
        }
    }

    /// A bitfield of `len` pieces, all present.
    #[must_use]
    pub fn full(len: u32) -> Self {
        let mut bf = Bitfield {
            bits: vec![0xFFu8; byte_len(len)],
            len,
        };
        bf.clear_spare_bits();
        bf
    }

    /// Wrap raw bytes as a bitfield of `len` pieces.
    ///
    /// # Errors
    ///
    /// The byte length must match `len` exactly, and spare trailing bits
    /// must be zero; either violation returns `Err`.
    pub fn from_bytes(bytes: &[u8], len: u32) -> Result<Self, BadBitfield> {
        if bytes.len() != byte_len(len) {
            return Err(BadBitfield::WrongLength);
        }
        let used = len % 8;
        if used != 0 {
            let spare = 0xFFu8 >> used;
            if bytes.last().is_some_and(|&b| b & spare != 0) {
                return Err(BadBitfield::SpareBitsSet);
            }
        }
        Ok(Bitfield {
            bits: bytes.to_vec(),
            len,
        })
    }

    /// The raw MSB-first bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Number of pieces this bitfield spans.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Whether the bitfield spans zero pieces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether piece `index` is present. Out-of-range indices are absent.
    #[must_use]
    pub fn has(&self, index: u32) -> bool {
        if index >= self.len {
            return false;
        }
        let byte = (index / 8) as usize;
        let bit = 7 - (index % 8);
        self.bits[byte] & (1 << bit) != 0
    }

    /// Mark piece `index` present. Out-of-range indices are ignored.
    pub fn set(&mut self, index: u32) {
        if index >= self.len {
            return;
        }
        let byte = (index / 8) as usize;
        let bit = 7 - (index % 8);
        self.bits[byte] |= 1 << bit;
    }

    /// Mark piece `index` absent. Out-of-range indices are ignored.
    pub fn clear(&mut self, index: u32) {
        if index >= self.len {
            return;
        }
        let byte = (index / 8) as usize;
        let bit = 7 - (index % 8);
        self.bits[byte] &= !(1 << bit);
    }

    /// How many pieces are present.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.bits.iter().map(|b| b.count_ones()).sum()
    }

    /// Whether every piece is present.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.count() == self.len
    }

    /// Iterate the indices of present pieces, ascending.
    pub fn iter_present(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| self.has(i))
    }

    fn clear_spare_bits(&mut self) {
        let used = self.len % 8;
        if used != 0
            && let Some(last) = self.bits.last_mut()
        {
            *last &= !(0xFFu8 >> used);
        }
    }
}

impl std::fmt::Debug for Bitfield {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitfield({}/{} present)", self.count(), self.len)
    }
}

/// Why raw bytes were rejected as a bitfield.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadBitfield {
    /// Byte length disagrees with the piece count.
    WrongLength,
    /// Trailing spare bits were set.
    SpareBitsSet,
}

/// Bytes needed to hold `len` bits.
#[must_use]
pub fn byte_len(len: u32) -> usize {
    len.div_ceil(8) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_query() {
        let mut bf = Bitfield::empty(10);
        assert!(!bf.is_empty());
        assert_eq!(bf.count(), 0);
        bf.set(0);
        bf.set(7);
        bf.set(9);
        assert!(bf.has(0) && bf.has(7) && bf.has(9));
        assert!(!bf.has(1) && !bf.has(8));
        assert!(!bf.has(10)); // out of range
        assert_eq!(bf.count(), 3);
        assert_eq!(bf.iter_present().collect::<Vec<_>>(), vec![0, 7, 9]);
    }

    #[test]
    fn clear_removes_one_bit_and_nothing_else() {
        let mut bf = Bitfield::full(10);
        bf.clear(7);
        assert!(!bf.has(7));
        assert!(bf.has(6) && bf.has(8));
        assert_eq!(bf.count(), 9);
        // Idempotent, and out-of-range is ignored rather than panicking.
        bf.clear(7);
        bf.clear(10);
        bf.clear(u32::MAX);
        assert_eq!(bf.count(), 9);
        // Spare bits stay zero: clearing must not disturb the final byte.
        assert_eq!(bf.as_bytes()[1] & 0x3F, 0);
    }

    #[test]
    fn full_clears_spare_bits() {
        let bf = Bitfield::full(10);
        assert!(bf.is_full());
        assert_eq!(bf.count(), 10);
        // 10 pieces -> 2 bytes, 6 spare bits must be zero.
        assert_eq!(bf.as_bytes(), &[0xFF, 0b1100_0000]);
    }

    #[test]
    fn round_trips_through_bytes() {
        let bf = Bitfield::full(17);
        let copy = Bitfield::from_bytes(bf.as_bytes(), 17).unwrap();
        assert_eq!(bf, copy);
    }

    #[test]
    fn rejects_bad_bytes() {
        assert_eq!(
            Bitfield::from_bytes(&[0xFF], 10),
            Err(BadBitfield::WrongLength)
        );
        assert_eq!(
            Bitfield::from_bytes(&[0xFF, 0xFF], 10),
            Err(BadBitfield::SpareBitsSet)
        );
        assert!(Bitfield::from_bytes(&[0xFF, 0b1100_0000], 10).is_ok());
    }

    #[test]
    fn zero_length() {
        let bf = Bitfield::empty(0);
        assert!(bf.is_empty());
        assert!(bf.is_full()); // vacuously
        assert_eq!(bf.as_bytes(), &[] as &[u8]);
    }
}
