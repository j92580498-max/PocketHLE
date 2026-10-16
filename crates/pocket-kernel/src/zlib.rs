//! Owned host zlib streams for WinCE's 32-bit z_stream ABI.
//! Unfinished Business loads its CFL assets through zlib.dll ordinals 22/19/20.
use flate2::{Compression, Decompress, FlushDecompress, Status};
use std::collections::HashMap;
use std::io::Write;

pub struct Step {
    pub consumed: usize,
    pub written: usize,
    pub status: i32,
    pub adler: u32,
}
struct Stream {
    decoder: Decompress,
    adler: u32,
}
#[derive(Default)]
pub struct ZlibStreams {
    next: u32,
    streams: HashMap<u32, Stream>,
}
impl ZlibStreams {
    pub fn init(&mut self) -> Option<u32> {
        if self.streams.len() >= 1024 {
            return None;
        }
        let mut handle = self.next.max(0x6e180000);
        while self.streams.contains_key(&handle) {
            handle = handle.wrapping_add(1).max(0x6e180000);
        }
        self.next = handle.wrapping_add(1).max(0x6e180000);
        self.streams.insert(
            handle,
            Stream {
                decoder: Decompress::new(true),
                adler: 1,
            },
        );
        Some(handle)
    }
    pub fn contains(&self, handle: u32) -> bool {
        self.streams.contains_key(&handle)
    }
    pub fn inflate(
        &mut self,
        handle: u32,
        input: &[u8],
        output: &mut [u8],
        flush: u32,
    ) -> Result<Step, i32> {
        let stream = self.streams.get_mut(&handle).ok_or(-2)?;
        let finish = flush == 4;
        let flush = match flush {
            0 | 1 | 3 => FlushDecompress::None,
            2 => FlushDecompress::Sync,
            4 => FlushDecompress::Finish,
            _ => return Err(-2),
        };
        let before_in = stream.decoder.total_in();
        let before_out = stream.decoder.total_out();
        let result = stream.decoder.decompress(input, output, flush);
        let consumed = (stream.decoder.total_in() - before_in) as usize;
        let written = (stream.decoder.total_out() - before_out) as usize;
        let mut a = stream.adler & 65535;
        let mut b = stream.adler >> 16;
        for &byte in &output[..written] {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        stream.adler = (b << 16) | a;
        let status = match result {
            Ok(Status::StreamEnd) => 1,
            Ok(Status::BufError) if !finish && (consumed != 0 || written != 0) => 0,
            Ok(Status::BufError) => -5,
            Ok(Status::Ok) if consumed == 0 && written == 0 => -5,
            Ok(Status::Ok) => 0,
            Err(error) if error.needs_dictionary().is_some() => 2,
            Err(_) => -3,
        };
        Ok(Step {
            consumed,
            written,
            status,
            adler: stream.adler,
        })
    }
    pub fn end(&mut self, handle: u32) -> bool {
        self.streams.remove(&handle).is_some()
    }
    pub fn compress(input: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input)?;
        encoder.finish()
    }
}
