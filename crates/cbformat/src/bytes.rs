//! The readers' one way to take integers and fields from bytes. None of it
//! can panic, whatever the bytes hold.
//!
//! - [`Fields`] reads a record whose size is fixed, `[u8; N]`, at offsets
//!   that are constants: a field that would reach past the record does not
//!   compile.
//! - [`array`] takes the bytes at an offset found at run time, `None` past
//!   the end.
//! - [`Cursor`] reads bytes in order, `None` past the end.

/// Fields at constant offsets of a record of `N` bytes. Each offset is
/// checked against `N` when the program is built.
pub(crate) trait Fields {
    /// The `W` bytes at `AT`.
    fn field<const AT: usize, const W: usize>(&self) -> &[u8; W];
    /// Writes `value` over the `W` bytes at `AT`.
    fn put<const AT: usize, const W: usize>(&mut self, value: [u8; W]);

    fn le_i16<const AT: usize>(&self) -> i16 {
        i16::from_le_bytes(*self.field::<AT, 2>())
    }
    fn le_u16<const AT: usize>(&self) -> u16 {
        u16::from_le_bytes(*self.field::<AT, 2>())
    }
    fn le_i32<const AT: usize>(&self) -> i32 {
        i32::from_le_bytes(*self.field::<AT, 4>())
    }
    fn le_u32<const AT: usize>(&self) -> u32 {
        u32::from_le_bytes(*self.field::<AT, 4>())
    }
    fn le_i64<const AT: usize>(&self) -> i64 {
        i64::from_le_bytes(*self.field::<AT, 8>())
    }
    fn le_u64<const AT: usize>(&self) -> u64 {
        u64::from_le_bytes(*self.field::<AT, 8>())
    }
    fn be_u16<const AT: usize>(&self) -> u16 {
        u16::from_be_bytes(*self.field::<AT, 2>())
    }
    fn be_u24<const AT: usize>(&self) -> u32 {
        let &[a, b, c] = self.field::<AT, 3>();
        u32::from_be_bytes([0, a, b, c])
    }
    fn be_i32<const AT: usize>(&self) -> i32 {
        i32::from_be_bytes(*self.field::<AT, 4>())
    }
    fn be_u32<const AT: usize>(&self) -> u32 {
        u32::from_be_bytes(*self.field::<AT, 4>())
    }
    fn be_i64<const AT: usize>(&self) -> i64 {
        i64::from_be_bytes(*self.field::<AT, 8>())
    }
    fn be_u64<const AT: usize>(&self) -> u64 {
        u64::from_be_bytes(*self.field::<AT, 8>())
    }
}

impl<const N: usize> Fields for [u8; N] {
    fn field<const AT: usize, const W: usize>(&self) -> &[u8; W] {
        const { assert!(AT <= N && W <= N - AT, "a field past the end of its record") };
        // Always inside, by the assertion; the zeros are never read.
        self.get(AT..).and_then(<[u8]>::first_chunk).unwrap_or(const { &[0; W] })
    }

    fn put<const AT: usize, const W: usize>(&mut self, value: [u8; W]) {
        const { assert!(AT <= N && W <= N - AT, "a field past the end of its record") };
        if let Some(field) = self.get_mut(AT..).and_then(<[u8]>::first_chunk_mut) {
            *field = value;
        }
    }
}

/// The `N` bytes of `b` from `at`, or `None` when they run past its end.
pub(crate) fn array<const N: usize>(b: &[u8], at: usize) -> Option<&[u8; N]> {
    b.get(at..)?.first_chunk()
}

/// Reads bytes in order. Each read takes the bytes it needs where the last
/// one ended, or gives `None`, and takes nothing, when they run past the end.
pub(crate) struct Cursor<'a> {
    b: &'a [u8],
    /// Never past the end of `b`.
    at: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Cursor { b, at: 0 }
    }

    /// Where the next read starts: the bytes read so far.
    pub(crate) fn at(&self) -> usize {
        self.at
    }

    /// The bytes not read yet.
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.b.get(self.at..).unwrap_or_default()
    }

    pub(crate) fn left(&self) -> usize {
        self.rest().len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.rest().get(..n)?;
        self.at += n;
        Some(s)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<&'a [u8; N]> {
        let s = self.rest().first_chunk()?;
        self.at += N;
        Some(s)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.array().map(|&[b]| b)
    }

    pub(crate) fn le_u16(&mut self) -> Option<u16> {
        self.array().map(|b| u16::from_le_bytes(*b))
    }

    pub(crate) fn be_u16(&mut self) -> Option<u16> {
        self.array().map(|b| u16::from_be_bytes(*b))
    }

    pub(crate) fn le_i32(&mut self) -> Option<i32> {
        self.array().map(|b| i32::from_le_bytes(*b))
    }

    pub(crate) fn le_u32(&mut self) -> Option<u32> {
        self.array().map(|b| u32::from_le_bytes(*b))
    }

    pub(crate) fn be_i32(&mut self) -> Option<i32> {
        self.array().map(|b| i32::from_be_bytes(*b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_at_constant_offsets() {
        let b = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        assert_eq!(b.be_u16::<1>(), 0x0203);
        assert_eq!(b.be_u24::<1>(), 0x02_0304);
        assert_eq!(b.be_u32::<1>(), 0x0203_0405);
        assert_eq!(b.le_i32::<1>(), 0x0504_0302);
        assert_eq!(b.le_u16::<7>(), 0x0908);
        assert_eq!(b.le_i16::<0>(), 0x0201);
        assert_eq!(b.le_u32::<5>(), 0x0908_0706);
        assert_eq!(b.le_u64::<1>(), 0x0908_0706_0504_0302);
        assert_eq!(b.le_i64::<0>(), 0x0807_0605_0403_0201);
        assert_eq!(b.be_i64::<1>(), 0x0203_0405_0607_0809);
        assert_eq!(b.be_u64::<0>(), 0x0102_0304_0506_0708);
        assert_eq!(b.be_i32::<5>(), 0x0607_0809);
        assert_eq!([0xff, 0xfe].le_i16::<0>(), -257);
        assert_eq!(b.field::<6, 3>(), &[7, 8, 9]);
        assert_eq!(b.field::<9, 0>(), &[]);
        let mut w = [0u8; 6];
        w.put::<2, 4>(0x0102_0304u32.to_be_bytes());
        assert_eq!(w, [0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn arrays_at_offsets_found_at_run_time() {
        let b = [1, 2, 3, 4];
        assert_eq!(array::<2>(&b, 2), Some(&[3, 4]));
        assert_eq!(array::<2>(&b, 3), None);
        assert_eq!(array::<0>(&b, 4), Some(&[]));
        assert_eq!(array::<1>(&b, usize::MAX), None);
    }

    #[test]
    fn a_cursor_takes_nothing_past_the_end() {
        let b = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let mut c = Cursor::new(&b);
        assert_eq!(c.u8(), Some(1));
        assert_eq!(c.le_u16(), Some(0x0302));
        assert_eq!((c.at(), c.left()), (3, 4));
        assert_eq!(c.be_i32(), Some(0x0405_0607));
        assert_eq!((c.u8(), c.le_i32(), c.take(1)), (None, None, None));
        assert_eq!((c.at(), c.rest()), (7, &[][..]));
        let mut c = Cursor::new(&b);
        assert_eq!(c.take(usize::MAX), None);
        assert_eq!(c.take(2), Some(&[1, 2][..]));
        assert_eq!(c.be_u16(), Some(0x0304));
        assert_eq!(c.le_i32(), None, "three bytes are left");
        assert_eq!(c.array::<3>(), Some(&[5, 6, 7]));
        assert_eq!(c.take(0), Some(&[][..]));
    }
}
