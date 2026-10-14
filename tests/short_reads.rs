use std::io::{self, Cursor, Read};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use icy_metadata::IcyMetadataReader;

struct Fragmented {
    inner: Cursor<Vec<u8>>,
    maximum: usize,
    fault: Option<(usize, io::ErrorKind)>,
}

impl Read for Fragmented {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        if let Some((position, kind)) = self.fault {
            if self.inner.position() as usize == position {
                self.fault = None;
                return Err(kind.into());
            }
        }
        let mut limit = buf.len().min(self.maximum);
        if let Some((position, _)) = self.fault {
            limit = limit.min(position.saturating_sub(self.inner.position() as usize));
        }
        self.inner.read(&mut buf[..limit])
    }
}

fn wire(units: u8) -> Vec<u8> {
    let mut data = b"abcd".to_vec();
    data.push(units);
    if units != 0 {
        let mut metadata = b"StreamTitle='x';".to_vec();
        metadata.resize(usize::from(units) * 16, 0);
        data.extend(metadata);
    }
    data.extend(b"efgh\0ij");
    data
}

fn run(data: Vec<u8>, maximum: usize, caller: usize, fault: Option<(usize, io::ErrorKind)>)
    -> (Vec<u8>, usize)
{
    let events = Arc::new(Mutex::new(0));
    let callback = events.clone();
    let input = Fragmented { inner: Cursor::new(data), maximum, fault };
    let mut reader = IcyMetadataReader::new(input, NonZeroUsize::new(4), move |metadata| {
        assert_eq!(metadata.unwrap().stream_title(), Some("x"));
        *callback.lock().unwrap() += 1;
    });
    let mut output = Vec::new();
    let mut buf = vec![0; caller];
    for _ in 0..20000 {
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        match reader.read(&mut buf) {
            Ok(0) => return (output, *events.lock().unwrap()),
            Ok(count) => output.extend_from_slice(&buf[..count]),
            Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => (),
            Err(error) => panic!("framing error: {error}; audio={output:?}"),
        }
    }
    panic!("reader failed to make bounded progress");
}

#[test]
fn i01_legal_one_byte_reads_preserve_audio() {
    let (audio, events) = run(b"abcd\0efgh\0".to_vec(), 1, 32, None);
    assert_eq!(audio, b"abcdefgh");
    assert_eq!(events, 0);
}

#[test]
fn i02_i03_fragmentation_small_buffers_and_metadata_lengths() {
    for units in [0, 1, 255] {
        for maximum in 1..=17 {
            for caller in [1, 2, 3, 4, 5, 7, 32, 8192] {
                let (audio, events) = run(wire(units), maximum, caller, None);
                assert_eq!(audio, b"abcdefghij");
                assert_eq!(events, usize::from(units != 0));
            }
        }
    }
}

#[test]
fn i04_temporary_errors_preserve_partial_metadata_and_audio() {
    let data = wire(1);
    for position in 0..data.len() {
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::WouldBlock] {
            let (audio, events) = run(data.clone(), 3, 32, Some((position, kind)));
            assert_eq!(audio, b"abcdefghij", "position={position}, kind={kind:?}");
            assert_eq!(events, 1);
        }
    }
}

#[test]
fn i05_truncated_metadata_is_not_eof_and_prior_audio_is_returned() {
    for cut in 4..21 {
        let mut reader = IcyMetadataReader::new(Cursor::new(wire(1)[..cut].to_vec()),
            NonZeroUsize::new(4), |_| panic!("truncated block emitted metadata"));
        let mut buf = [0; 64];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"abcd");
        assert_eq!(reader.read(&mut buf).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[test]
fn i07_invalid_metadata_text_does_not_corrupt_audio() {
    let mut data = wire(1);
    data[5] = 255;
    let errors = Arc::new(Mutex::new(0));
    let callback = errors.clone();
    let mut reader = IcyMetadataReader::new(Cursor::new(data), NonZeroUsize::new(4), move |value| {
        assert!(value.is_err());
        *callback.lock().unwrap() += 1;
    });
    let mut audio = Vec::new();
    reader.read_to_end(&mut audio).unwrap();
    assert_eq!(audio, b"abcdefghij");
    assert_eq!(*errors.lock().unwrap(), 1);
}
