// if anybody knows how to put this on just the ffi module please let me know
#![allow(unused)]

use anyhow::{anyhow, Error};
use cast::u64;
use core::slice;
use std::io::{self, Read, Seek, SeekFrom, Write};

use autocxx::{prelude::*, subclass::*};

use self::ffi::{
    bsdiff::{self, FileInterface, FileInterface_methods},
    StreamAdapterCpp,
};

use super::{StreamRead, StreamWrite};

include_cpp! {
    #include "bsdiff/file_interface.h"
    #include "bsdiff/bspatch.h"
    generate_ns!("bsdiff")
}

enum Stream {
    Read(*mut dyn StreamRead),
    Write(*mut dyn StreamWrite),
}

impl Stream {
    unsafe fn seek_unsafe(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Stream::Read(inner) => (**inner).seek(pos),
            Stream::Write(inner) => (**inner).seek(pos),
        }
    }

    // stolen from the standard library (unstable)
    unsafe fn stream_len_unsafe(&mut self) -> io::Result<u64> {
        let old_pos = self.seek_unsafe(SeekFrom::Current(0))?;
        let len = self.seek_unsafe(SeekFrom::End(0))?;

        // Avoid seeking a third time when we were already at the end of the
        // stream. The branch is usually way cheaper than a seek operation.
        if old_pos != len {
            self.seek_unsafe(SeekFrom::Start(old_pos))?;
        }

        Ok(len)
    }

    fn new_reader(inner: &mut (impl Read + Seek)) -> Self {
        Stream::Read(
            inner as &mut dyn StreamRead as *mut dyn StreamRead as *mut (dyn StreamRead + 'static),
        )
    }

    fn new_writer(inner: &mut (impl Write + Seek)) -> Self {
        Stream::Write(
            inner as &mut dyn StreamWrite as *mut dyn StreamWrite
                as *mut (dyn StreamWrite + 'static),
        )
    }
}

#[subclass(superclass("bsdiff::FileInterface"))]
pub struct StreamAdapter {
    inner: Stream,
    err_ptr: *mut Option<Error>,
}

impl CppPeerConstructor<StreamAdapterCpp> for StreamAdapter {
    fn make_peer(
        &mut self,
        peer_holder: CppSubclassRustPeerHolder<Self>,
    ) -> UniquePtr<StreamAdapterCpp> {
        UniquePtr::emplace(unsafe { StreamAdapterCpp::new(peer_holder) })
    }
}

impl StreamAdapter {
    fn new(inner: Stream, err_ptr: *mut Option<Error>) -> Self {
        Self { inner, err_ptr, cpp_peer: Default::default() }
    }

    fn to_file_interface(self) -> UniquePtr<FileInterface> {
        StreamAdapter::as_FileInterface_unique_ptr(StreamAdapter::new_cpp_owned(self))
    }

    unsafe fn record_err<T, E: Into<Error>>(&mut self, result: Result<T, E>) -> Option<T> {
        match result {
            Ok(val) => return Some(val),
            Err(err) => *(&mut *self.err_ptr) = Some(err.into()),
        }
        None
    }
}

#[allow(non_snake_case)]
impl FileInterface_methods for StreamAdapter {
    unsafe fn Read(&mut self, buf_ptr: *mut c_void, count: usize, bytes_read: *mut usize) -> bool {
        if let Stream::Read(reader) = &mut self.inner {
            let buf = slice::from_raw_parts_mut(buf_ptr as *mut u8, count);
            let result = (**reader).read(buf);
            if let Some(amount) = self.record_err(result) {
                *bytes_read = amount;
                return true;
            }
        }
        false
    }

    unsafe fn Write(
        &mut self,
        buf_ptr: *const c_void,
        count: usize,
        bytes_written: *mut usize,
    ) -> bool {
        if let Stream::Write(writer) = &mut self.inner {
            let buf = slice::from_raw_parts(buf_ptr as *const u8, count);
            let result = (**writer).write(buf);
            if let Some(amount) = self.record_err(result) {
                *bytes_written = amount;
                return true;
            }
        }
        false
    }

    unsafe fn Seek(&mut self, pos: c_long) -> bool {
        if let Ok(pos) = u64(pos.0) {
            let result = self.inner.seek_unsafe(SeekFrom::Start(pos));
            return self.record_err(result).is_some();
        }
        false
    }

    unsafe fn Close(&mut self) -> bool {
        if let Stream::Write(writer) = &mut self.inner {
            let result = (**writer).flush();
            return self.record_err(result).is_some();
        }
        true
    }

    unsafe fn GetSize(&mut self, size_ptr: *mut u64) -> bool {
        let result = self.inner.stream_len_unsafe();
        if let Some(size) = self.record_err(result) {
            *size_ptr = size;
            return true;
        }
        false
    }
}

pub fn bspatch(
    src: &mut (impl Read + Seek),
    dst: &mut (impl Write + Seek),
    data: &[u8],
) -> anyhow::Result<()> {
    let mut src_err = None;
    let mut dst_err = None;

    let src = StreamAdapter::new(Stream::new_reader(src), &mut src_err).to_file_interface();
    let dst = StreamAdapter::new(Stream::new_writer(dst), &mut dst_err).to_file_interface();

    let res = unsafe { bsdiff::bspatch3(&src, &dst, data.as_ptr(), data.len()) };

    match res.0 {
        0 => Ok(()),
        1 => Err(src_err.or(dst_err).unwrap_or(anyhow!("Unknown IO error ocurred"))),
        2 => Err(anyhow!("Invalid bspatch data")),
        _ => Err(anyhow!("Unknown error ocurred")),
    }
}

mod tests {
    use std::{
        fs::{self, File},
        io::{self, Cursor, Seek, Write},
    };

    use anyhow::anyhow;

    use super::bspatch;

    #[test]
    fn bspatch_test() {
        let mut old = File::open("test/bin1").unwrap();
        let patch = fs::read("test/patch").unwrap();
        let mut new_vec = vec![];
        let mut new = Cursor::new(&mut new_vec);
        bspatch(&mut old, &mut new, &patch).unwrap();

        let new_correct = fs::read("test/bin2").unwrap();
        // don't use assert_eq since these vectors are big
        assert!(new_vec == new_correct);
    }

    #[test]
    fn bspatch_io_err_test() {
        struct BadWriter<T: Write + Seek> {
            inner: T,
            limit: usize,
            count: usize,
        }
        impl<T: Write + Seek> Write for BadWriter<T> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.count > self.limit {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, anyhow!("Oh no!")))
                } else {
                    let result = self.inner.write(buf);
                    if let Ok(amount) = result {
                        self.count += amount;
                    }
                    result
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.inner.flush()
            }
        }
        impl<T: Write + Seek> Seek for BadWriter<T> {
            fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
                self.inner.seek(pos)
            }
        }

        let mut old = File::open("test/bin1").unwrap();
        let new_correct = fs::read("test/bin2").unwrap();
        let patch = fs::read("test/patch").unwrap();
        let mut new_vec = vec![];
        let mut new =
            BadWriter { inner: Cursor::new(&mut new_vec), limit: new_correct.len() / 2, count: 0 };
        let err = bspatch(&mut old, &mut new, &patch);
        assert!(err.is_err())
    }
}
