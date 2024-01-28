use crate::update_metadata::Extent as RawExtent;
use anyhow::{anyhow, bail, Result};
use cast::{i64, u64, usize};
use std::{
    cmp::min,
    io::{self, Read, Seek, SeekFrom, Write},
    iter,
};

use super::calculate_rel;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Extent {
    pub start: usize,
    pub len: usize,
}

/// terminology:
/// - inner position: the position as seen by the inner stream
/// - outer position: the position as seen by users of the ExtentStream
/// the inner position jumps around while the outer position is contiguous
///
/// notes:
/// - extents must be sorted and disjoint
/// - seeking past the end of the inner stream won't necessarily error,
///   but seeking past the end of the extents will error
/// - if the stream ends before the extents do, then seek will use the shorter one for SeekFrom::End.
///   for example, if the extents are [0..20, 40..60] but the stream is only of length 45,
///   then seek(SeekFrom::End(0)) will return 25 (0..20 + 40..45)
/// - if no extents are specified then new returns none
pub struct ExtentStream<T: Seek> {
    inner: T,
    cursor: (usize, usize),
    extents: Vec<Extent>,
    /// `extents_outer[i]` is the outer starting position of the ith extent
    /// we also make `extents_outer[extents.len()]` the (exclusive) end of the last extent
    /// thus the ith extent goes from `extents_outer[i]` to `extents_outer[i + 1]` (exclusive)
    extents_outer: Vec<usize>,
}

enum NextArea {
    CurrentExtent(usize), // number of bytes remaining in current extent
    NextExtent(usize),    // index of next extent (must be valid!)
    None,
}

impl<T: Seek> ExtentStream<T> {
    pub fn new(inner: T, extents: Vec<Extent>) -> io::Result<Option<Self>> {
        if extents.is_empty() {
            return Ok(None);
        }

        let mut result = Self {
            inner,
            cursor: (0, 0),
            extents_outer: iter::once(0)
                .chain(extents.iter().map(|extent| extent.len).scan(0, |sum, e| {
                    *sum += e;
                    Some(*sum)
                }))
                .collect(),
            extents,
        };
        result.set_cursor(0, 0)?;

        Ok(Some(result))
    }

    pub fn new_range(inner: T, start: usize, len: usize) -> io::Result<Self> {
        Self::new(inner, vec![Extent { start, len }]).map(Option::unwrap)
    }

    pub fn new_suffix(inner: T, start: usize) -> io::Result<Self> {
        Self::new(inner, vec![Extent { start, len: usize::MAX / 2 - start }]).map(Option::unwrap)
    }

    /// warning: this will not necessarily be the same as the length reported by Seek::stream_len,
    /// this is because this method reports the length as specified by the extents,
    /// whereas the underlying stream might end before the extents do which will be reflected in seeking
    pub fn len(&self) -> usize {
        *self.extents_outer.last().unwrap()
    }

    fn next_area(&self) -> NextArea {
        let (extent_i, byte_i) = self.cursor;
        if extent_i >= self.extents.len() {
            return NextArea::None;
        }

        let extent_len = self.extents[extent_i].len;
        let extent_rem =
            extent_len.checked_sub(byte_i).expect("internal error: extent index > extent size");

        if extent_rem > 0 {
            NextArea::CurrentExtent(extent_rem)
        } else if extent_i + 1 < self.extents.len() {
            NextArea::NextExtent(extent_i + 1)
        } else {
            NextArea::None
        }
    }

    fn set_cursor(&mut self, extent_i: usize, byte_i: usize) -> io::Result<u64> {
        self.cursor = (extent_i, byte_i);
        self.inner.seek(SeekFrom::Start(u64(self.extents[extent_i].start + byte_i)))?;
        Ok(u64(self.extents_outer[extent_i] + byte_i))
    }

    fn find_cursor_outer(&self, outer_pos: usize) -> Option<(usize, usize)> {
        for i in 0..self.extents.len() {
            if self.extents_outer[i] <= outer_pos && outer_pos < self.extents_outer[i + 1] {
                return Some((i, outer_pos - self.extents_outer[i]));
            }
        }
        if outer_pos == self.len() {
            // we are at the very end
            return Some((self.extents.len() - 1, self.extents.last().unwrap().len));
        }
        None
    }
}

impl<T: Seek> Seek for ExtentStream<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let err_before_start = |pos| {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                anyhow!("Attempted to seek before the start of extent stream (pos = {} < 0)", pos),
            ))
        };
        match pos {
            SeekFrom::Start(pos) => {
                let pos = usize(pos);
                if let Some((extent_i, byte_i)) = self.find_cursor_outer(pos) {
                    self.set_cursor(extent_i, byte_i)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        anyhow!(
                            "Attempted to seek past end of extent stream (pos = {} > {} = end)",
                            pos,
                            self.len()
                        ),
                    ))
                }
            }
            SeekFrom::End(offset) => {
                let inner_len = usize(self.inner.seek(SeekFrom::End(0))?);
                let mut inner_len_outer = 0;
                for i in 0..self.extents.len() {
                    let extent = self.extents[i];
                    if extent.start + extent.len <= inner_len {
                        inner_len_outer += extent.len;
                    } else {
                        if extent.start < inner_len {
                            inner_len_outer += inner_len - extent.start;
                        }
                        break;
                    }
                }
                let inner_end = min(self.len(), inner_len_outer);
                match calculate_rel(0, u64(inner_end), i64(offset)) {
                    Ok(pos) => self.seek(SeekFrom::Start(pos)),
                    Err(pos) => err_before_start(pos),
                }
            }
            SeekFrom::Current(offset) => {
                let inner_pos = u64(self.extents_outer[self.cursor.0] + self.cursor.1);
                match calculate_rel(0, inner_pos, i64(offset)) {
                    Ok(pos) => self.seek(SeekFrom::Start(pos)),
                    Err(pos) => err_before_start(pos),
                }
            }
        }
    }
}

impl<T: Read + Seek> Read for ExtentStream<T> {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut total = 0;
        while !buf.is_empty() {
            match self.next_area() {
                NextArea::CurrentExtent(rem) => {
                    let max_len = min(buf.len(), rem);
                    let len = self.inner.read(&mut buf[..max_len])?;
                    self.cursor.1 += len;

                    buf = &mut buf[len..];
                    total += len;
                    if len == 0 {
                        break;
                    }
                }
                NextArea::NextExtent(index) => {
                    self.set_cursor(index, 0)?;
                }
                NextArea::None => break,
            }
        }
        Ok(total)
    }
}

impl<T: Write + Seek> Write for ExtentStream<T> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let mut total = 0;
        while !buf.is_empty() {
            match self.next_area() {
                NextArea::CurrentExtent(rem) => {
                    let max_len = min(buf.len(), rem);
                    let len = self.inner.write(&buf[..max_len])?;
                    self.cursor.1 += len;

                    buf = &buf[len..];
                    total += len;
                    if len == 0 {
                        break;
                    }
                }
                NextArea::NextExtent(index) => {
                    self.set_cursor(index, 0)?;
                }
                NextArea::None => break,
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn convert_extent(extent: &RawExtent, block_size: usize) -> Result<Extent> {
    if extent.start_block == Some(u64::MAX) {
        bail!("Sparse holes are not supported (I don't know what they are :/)");
    }

    Ok(Extent {
        start: block_size
            * usize(extent.start_block.ok_or_else(|| anyhow!("Missing start_block in extent"))?),
        len: block_size
            * usize(extent.num_blocks.ok_or_else(|| anyhow!("Missing num_block in extent"))?),
    })
}

pub fn convert_extents(extents: &[RawExtent], block_size: usize) -> Result<Vec<Extent>> {
    if block_size == 0 {
        bail!("Block size cannot be 0")
    }
    extents.iter().map(|extent| convert_extent(extent, block_size)).collect()
}

#[cfg(test)]
mod tests {
    use cast::u8;
    use once_cell::sync::Lazy;
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};

    use super::{convert_extents, ExtentStream};
    use crate::{extract::extent::Extent, update_metadata::Extent as RawExtent};

    static RAW_EXTENTS: Lazy<Vec<RawExtent>> = Lazy::new(|| {
        vec![(0, 4), (6, 5), (20, 13), (80, 100)]
            .into_iter()
            .map(|(start_block, num_blocks)| RawExtent {
                start_block: Some(start_block),
                num_blocks: Some(num_blocks),
            })
            .collect::<Vec<_>>()
    });

    const BLOCK_SIZE: usize = 3;

    #[test]
    fn extent_converter_test() {
        let extents = convert_extents(RAW_EXTENTS.as_slice(), BLOCK_SIZE).unwrap();
        assert_eq!(
            extents,
            vec![(0, 12), (18, 15), (60, 39), (240, 300)]
                .into_iter()
                .map(|(start, len)| Extent { start, len })
                .collect::<Vec<_>>()
        )
    }

    #[test]
    fn extent_converter_fail_test() {
        let mut raw_extents = RAW_EXTENTS.clone();
        raw_extents[2].start_block = None;
        assert!(convert_extents(raw_extents.as_slice(), BLOCK_SIZE).is_err());

        let mut raw_extents = RAW_EXTENTS.clone();
        raw_extents[2].num_blocks = None;
        assert!(convert_extents(raw_extents.as_slice(), BLOCK_SIZE).is_err());

        assert!(convert_extents(RAW_EXTENTS.as_slice(), 0).is_err());
    }

    static EXTENTS: Lazy<Vec<Extent>> = Lazy::new(|| {
        vec![(0, 3), (5, 2), (7, 3), (20, 5)]
            .into_iter()
            .map(|(start, len)| Extent { start, len })
            .collect::<Vec<_>>()
    });
    static EXTENTS_INNER_LEN: Lazy<usize> = Lazy::new(|| {
        let last = EXTENTS.last().unwrap();
        last.start + last.len
    });

    #[test]
    fn extent_stream_read_test() {
        let src =
            (0_u8..u8(*EXTENTS_INNER_LEN + 10).unwrap()).map(|i| 2 * i + 1).collect::<Vec<_>>();
        let mut stream = ExtentStream::new(Cursor::new(&src), EXTENTS.clone()).unwrap().unwrap();
        let mut dst = vec![];
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 13);
        assert_eq!(dst, [1, 3, 5, 11, 13, 15, 17, 19, 41, 43, 45, 47, 49]);
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 0);
    }

    #[test]
    fn extent_stream_write_test() {
        let src = (0_u8..13_u8).map(|i| 2 * i + 1).collect::<Vec<_>>();
        let mut dst = vec![0_u8; *EXTENTS_INNER_LEN];
        let mut stream =
            ExtentStream::new(Cursor::new(&mut dst), EXTENTS.clone()).unwrap().unwrap();
        stream.write_all(&src).unwrap();
        assert_eq!(stream.write(&src).unwrap(), 0);

        assert_eq!(
            dst,
            [1, 3, 5, 0, 0, 7, 9, 11, 13, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 19, 21, 23, 25]
        );
    }

    #[test]
    fn extent_stream_seek_rw_test() {
        let mut data = vec![0; *EXTENTS_INNER_LEN];
        let mut stream =
            ExtentStream::new(Cursor::new(&mut data), EXTENTS.clone()).unwrap().unwrap();

        // write
        assert_eq!(stream.seek(SeekFrom::Start(7)).unwrap(), 7);
        stream.write_all(&[10, 11]).unwrap();
        stream.write_all(&[13, 14]).unwrap();
        assert_eq!(stream.seek(SeekFrom::Current(-7)).unwrap(), 4);
        stream.write_all(&[16, 17]).unwrap();
        assert_eq!(stream.seek(SeekFrom::End(-2)).unwrap(), 11);
        stream.write_all(&[19, 20]).unwrap();
        assert_eq!(stream.write(&[21]).unwrap(), 0);

        // read
        assert_eq!(stream.seek(SeekFrom::Start(5)).unwrap(), 5);
        let mut dst = vec![];
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 8);
        assert_eq!(dst, [17, 0, 10, 11, 13, 14, 19, 20]);

        // write
        assert_eq!(stream.seek(SeekFrom::End(-7)).unwrap(), 6);
        stream.write_all(&[22, 23]).unwrap();

        // read
        assert_eq!(stream.seek(SeekFrom::Start(5)).unwrap(), 5);
        let mut dst = vec![];
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 8);
        assert_eq!(dst, [17, 22, 23, 11, 13, 14, 19, 20]);

        let mut target = vec![0_u8; *EXTENTS_INNER_LEN];
        let changes = vec![
            (9, 10),
            (20, 11),
            (21, 13),
            (22, 14),
            (6, 16),
            (7, 17),
            (23, 19),
            (24, 20),
            (8, 22),
            (9, 23),
        ];
        for (i, v) in changes {
            target[i] = v;
        }
        assert_eq!(data, target);
    }

    #[test]
    fn extent_stream_too_short_read_test() {
        let src = (0_u8..21_u8).map(|i| 2 * i + 1).collect::<Vec<_>>();
        let mut stream =
            ExtentStream::new(Cursor::new(src.as_slice()), EXTENTS.clone()).unwrap().unwrap();
        let mut dst = vec![];
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 9);
        assert_eq!(dst, [1, 3, 5, 11, 13, 15, 17, 19, 41]);
        assert_eq!(stream.read_to_end(&mut dst).unwrap(), 0);
    }

    #[test]
    fn extent_stream_too_short_write_test() {
        let src = (0_u8..13_u8).map(|i| 2 * i + 1).collect::<Vec<_>>();
        let mut dst = vec![0; 9];
        let mut stream =
            ExtentStream::new(Cursor::new(dst.as_mut_slice()), EXTENTS.clone()).unwrap().unwrap();
        assert_eq!(stream.write(&src).unwrap(), 7);
        assert_eq!(stream.write(&src).unwrap(), 0);

        assert_eq!(dst, [1, 3, 5, 0, 0, 7, 9, 11, 13]);
    }

    #[test]
    fn extent_stream_too_short_seek_test() {
        let data = vec![0; 27];
        let mut stream = ExtentStream::new(Cursor::new(&data), vec![Extent { start: 10, len: 20 }])
            .unwrap()
            .unwrap();
        assert_eq!(stream.seek(SeekFrom::End(0)).unwrap(), 17);
    }

    #[test]
    fn extent_stream_seek_fail_test() {
        let data = vec![0_u8; *EXTENTS_INNER_LEN];
        let mut stream = ExtentStream::new(Cursor::new(&data), EXTENTS.clone()).unwrap().unwrap();

        // start
        assert!(stream.seek(SeekFrom::Start(0)).is_ok());
        assert!(stream.seek(SeekFrom::Start(5)).is_ok());
        assert!(stream.seek(SeekFrom::Start(13)).is_ok());
        assert!(stream.seek(SeekFrom::Start(14)).is_err());
        assert!(stream.seek(SeekFrom::Start(20)).is_err());

        // end
        assert!(stream.seek(SeekFrom::End(-15)).is_err());
        assert!(stream.seek(SeekFrom::End(-14)).is_err());
        assert!(stream.seek(SeekFrom::End(-13)).is_ok());
        assert!(stream.seek(SeekFrom::End(-5)).is_ok());
        assert!(stream.seek(SeekFrom::End(0)).is_ok());
        assert!(stream.seek(SeekFrom::End(1)).is_err());

        // current
        assert!(stream.seek(SeekFrom::Start(5)).is_ok());
        assert!(stream.seek(SeekFrom::Current(-7)).is_err());
        assert!(stream.seek(SeekFrom::Current(-6)).is_err());
        assert!(stream.seek(SeekFrom::Current(-5)).is_ok());

        assert!(stream.seek(SeekFrom::Start(5)).is_ok());
        assert!(stream.seek(SeekFrom::Current(-3)).is_ok());

        assert!(stream.seek(SeekFrom::Start(5)).is_ok());
        assert!(stream.seek(SeekFrom::Current(8)).is_ok());

        assert!(stream.seek(SeekFrom::Start(5)).is_ok());
        assert!(stream.seek(SeekFrom::Current(9)).is_err());
    }
}
