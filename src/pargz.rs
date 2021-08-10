//! Parallel Gzip compression.
//!
//! # Examples
//!
//! ```
//! use std::{env, fs::File, io::Write};
//!
//! use gzp::pargz::ParGz;
//!
//! let mut writer = vec![];
//! let mut par_gz = ParGz::builder(writer).build();
//! par_gz.write_all(b"This is a first test line\n").unwrap();
//! par_gz.write_all(b"This is a second test line\n").unwrap();
//! par_gz.finish().unwrap();
//! ```
use std::io::{self, Read, Write};

use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use bytes::BytesMut;
use flate2::read::GzEncoder;
pub use flate2::Compression;
use flume::{bounded, Receiver, Sender};
use futures::executor::block_on;

use crate::{GzpError, BUFSIZE};

// use tokio::sync::mpsc::{self, Receiver, Sender};

/// The [`ParGz`] builder.
#[derive(Debug)]
pub struct ParGzBuilder<W> {
    /// The buffersize accumulate before trying to compress it. Defaults to [`BUFSIZE`].
    buffer_size: usize,
    /// The underlying writer to write to.
    writer: W,
    /// The number of threads to use for compression. Defaults to all available threads.
    num_threads: usize,
    /// The compression level of the output, see [`Compression`].
    compression_level: Compression,
}

impl<W> ParGzBuilder<W>
where
    W: Send + Write + 'static,
{
    /// Create a new [`ParGzBuilder`] object.
    pub fn new(writer: W) -> Self {
        Self {
            buffer_size: BUFSIZE,
            writer,
            num_threads: num_cpus::get(),
            compression_level: Compression::new(3),
        }
    }

    /// Set the [`buffer_size`](ParGzBuilder.buffer_size).
    pub fn buffer_size(mut self, buffer_size: usize) -> Self {
        assert!(buffer_size > 0);
        self.buffer_size = buffer_size;
        self
    }

    /// Set the [`num_threads`](ParGzBuilder.num_threads).
    ///
    /// gzp requires at least 4 threads:
    ///
    /// - 1 for the runtime itself
    /// - 1 for the compressor coordinator
    /// - 1 for the writer
    /// - 1 or more for doing compression
    pub fn num_threads(mut self, num_threads: usize) -> Self {
        assert!(num_threads <= num_cpus::get() && num_threads > 3);
        self.num_threads = num_threads;
        self
    }

    /// Set the [`compression_level`](ParGzBuilder.compression_level).
    pub fn compression_level(mut self, compression_level: Compression) -> Self {
        self.compression_level = compression_level;
        self
    }

    /// Create a configured [`ParGz`] object.
    pub fn build(self) -> ParGz {
        let (tx, rx) = bounded(self.num_threads);
        let buffer_size = self.buffer_size;
        let comp_level = self.compression_level;
        let handle = std::thread::spawn(move || {
            ParGz::run(rx, self.writer, self.num_threads - 1, comp_level)
        });
        ParGz {
            handle,
            tx,
            buffer: BytesMut::with_capacity(buffer_size),
            buffer_size,
        }
    }
}

pub struct ParGz {
    handle: std::thread::JoinHandle<Result<(), GzpError>>,
    tx: Sender<BytesMut>,
    buffer: BytesMut,
    buffer_size: usize,
}

impl ParGz {
    /// Create a builder to configure the [`ParGz`] runtime.
    pub fn builder<W>(writer: W) -> ParGzBuilder<W>
    where
        W: Write + Send + 'static,
    {
        ParGzBuilder::new(writer)
    }

    /// Launch the tokio runtime that coordinates the threadpool that does the following:
    ///
    /// 1. Receives chunks of bytes from from the [`ParGz::write`] method.
    /// 2. Spawn a task compressing the chunk of bytes.
    /// 3. Send the future for that task to the writer.
    /// 4. Write the bytes to the underlying writer.
    fn run<W>(
        rx: Receiver<BytesMut>,
        mut writer: W,
        num_threads: usize,
        compression_level: Compression,
    ) -> Result<(), GzpError>
    where
        W: Write + Send + 'static,
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap();

        pool.scope(move |s| {
            let (out_sender, out_receiver) = bounded(num_threads * 2);
            s.spawn(move |_s| {
                // writer
                while let Ok(chunk_chan) = out_receiver.recv() {
                    let chunk_chan: Receiver<Result<Vec<u8>, GzpError>> = chunk_chan;
                    let chunk = chunk_chan.recv().unwrap().unwrap();
                    writer.write_all(&chunk).unwrap();
                }
                writer.flush().unwrap();
            });

            while let Ok(chunk) = rx.recv() {
                let (buf_send, buf_recv) = bounded(1);
                s.spawn(move |_s| {
                    let mut buffer = Vec::with_capacity(chunk.len());
                    let mut encoder = GzEncoder::new(&chunk[..], compression_level);
                    encoder.read_to_end(&mut buffer).unwrap();

                    buf_send.send(Ok::<Vec<u8>, GzpError>(buffer)).unwrap();
                });
                out_sender.send(buf_recv).unwrap();
            }
        });

        Ok(())
    }

    /// Flush the buffers and wait on all threads to finish working.
    ///
    /// This *MUST* be called before the [`ParGz`] object goes out of scope.
    pub fn finish(mut self) -> Result<(), GzpError> {
        self.flush()?;
        drop(self.tx);
        let r = self.handle.join().unwrap();
        r
    }
}

impl Write for ParGz {
    /// Write a buffer into this writer, returning how many bytes were written.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        if self.buffer.len() > self.buffer_size {
            let b = self.buffer.split_to(self.buffer_size);
            self.tx
                .send(b)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.buffer
                .reserve(self.buffer_size.saturating_sub(self.buffer.len()))
        }

        Ok(buf.len())
    }

    /// Flush this output stream, ensuring all intermediately buffered contents are sent.
    fn flush(&mut self) -> std::io::Result<()> {
        self.tx
            .send(self.buffer.split())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{
        fs::File,
        io::{BufReader, BufWriter},
    };

    use flate2::bufread::MultiGzDecoder;
    use proptest::prelude::*;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_simple() {
        let dir = tempdir().unwrap();

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());

        // Define input bytes
        let input = b"
        This is a longer test than normal to come up with a bunch of text.
        We'll read just a few lines at a time.
        ";

        // Compress input to output
        let mut par_gz = ParGz::builder(out_writer).build();
        par_gz.write_all(input).unwrap();
        par_gz.finish().unwrap();

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut gz = MultiGzDecoder::new(&result[..]);
        let mut bytes = vec![];
        gz.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
    }

    #[test]
    fn test_regression() {
        let dir = tempdir().unwrap();

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());

        // Define input bytes that is 206 bytes long
        let input = [
            132, 19, 107, 159, 69, 217, 180, 131, 224, 49, 143, 41, 194, 30, 151, 22, 55, 30, 42,
            139, 219, 62, 123, 44, 148, 144, 88, 233, 199, 126, 110, 65, 6, 87, 51, 215, 17, 253,
            22, 63, 110, 1, 100, 202, 44, 138, 187, 226, 50, 50, 218, 24, 193, 218, 43, 172, 69,
            71, 8, 164, 5, 186, 189, 215, 151, 170, 243, 235, 219, 103, 1, 0, 102, 80, 179, 95,
            247, 26, 168, 147, 139, 245, 177, 253, 94, 82, 146, 133, 103, 223, 96, 34, 128, 237,
            143, 182, 48, 201, 201, 92, 29, 172, 137, 70, 227, 98, 181, 246, 80, 21, 106, 175, 246,
            41, 229, 187, 87, 65, 79, 63, 115, 66, 143, 251, 41, 251, 214, 7, 64, 196, 27, 180, 42,
            132, 116, 211, 148, 44, 177, 137, 91, 119, 245, 156, 78, 24, 253, 69, 38, 52, 152, 115,
            123, 94, 162, 72, 186, 239, 136, 179, 11, 180, 78, 54, 217, 120, 173, 141, 114, 174,
            220, 160, 223, 184, 114, 73, 148, 120, 43, 25, 21, 62, 62, 244, 85, 87, 19, 174, 182,
            227, 228, 70, 153, 5, 92, 51, 161, 9, 140, 199, 244, 241, 151, 236, 81, 211,
        ];

        // Compress input to output
        let mut par_gz = ParGz::builder(out_writer)
            .buffer_size(205)
            // .compression_level(Compression::new(2))
            .build();
        par_gz.write_all(&input).unwrap();
        par_gz.finish().unwrap();

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut gz = MultiGzDecoder::new(&result[..]);
        let mut bytes = vec![];
        gz.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
    }

    proptest! {
        #[test]
        fn test_all(
            input in prop::collection::vec(0..u8::MAX, 1..10_000),
            buf_size in 1..10_000usize,
            num_threads in 4..num_cpus::get(),
            write_size in 1..10_000usize,
        ) {
            let dir = tempdir().unwrap();

            // Create output file
            let output_file = dir.path().join("output.txt");
            let out_writer = BufWriter::new(File::create(&output_file).unwrap());


            // Compress input to output
            let mut par_gz = ParGz::builder(out_writer)
                .buffer_size(buf_size)
                .num_threads(num_threads)
                .build();
            for chunk in input.chunks(write_size) {
                par_gz.write_all(chunk).unwrap();
            }
            par_gz.finish().unwrap();

            // Read output back in
            let mut reader = BufReader::new(File::open(output_file).unwrap());
            let mut result = vec![];
            reader.read_to_end(&mut result).unwrap();

            // Decompress it
            let mut gz = MultiGzDecoder::new(&result[..]);
            let mut bytes = vec![];
            gz.read_to_end(&mut bytes).unwrap();

            // Assert decompressed output is equal to input
            assert_eq!(input.to_vec(), bytes);
        }
    }
}
