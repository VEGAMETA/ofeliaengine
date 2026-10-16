//! Sounds read and decoded off the game thread (ADR 0105): a thread answers the sound
//! system's requests, so no file read, decode or seek ever holds up a frame.

use engine_audio::{Clip, PreparedStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

/// What a sound file turned out to be.
pub(crate) enum Loaded {
    /// Short (or a WAV file): decoded whole, played from memory.
    Clip(Clip),
    /// Long: streamed from its file every time it plays.
    Long {
        /// Its sample rate in Hz.
        rate: u32,
    },
}

/// A request to the loading thread.
pub(crate) enum Request {
    /// Reads sound `id`: decoded whole when shorter than `stream_from` seconds.
    Load {
        id: u32,
        path: PathBuf,
        stream_from: f32,
    },
    /// Opens a long sound for one play from `offset` (frames at its rate).
    Prepare {
        ticket: u64,
        path: PathBuf,
        offset: u64,
        looping: bool,
        buffer_ms: u32,
    },
}

/// What the loading thread made.
pub(crate) enum Done {
    Loaded {
        id: u32,
        result: Result<Loaded, String>,
    },
    Prepared {
        ticket: u64,
        result: Result<PreparedStream, String>,
    },
}

/// The loading thread (`sound-load`), started with the sound system and joined when it is
/// dropped.
pub(crate) struct Loader {
    requests: Option<Sender<Request>>,
    done: Receiver<Done>,
    thread: Option<JoinHandle<()>>,
}

impl Loader {
    pub(crate) fn start() -> Self {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, done) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(String::from("sound-load"))
            .spawn(move || {
                engine_profiler::set_thread_name("sound-load");
                for request in inbox {
                    let answer = match request {
                        Request::Load {
                            id,
                            path,
                            stream_from,
                        } => Done::Loaded {
                            id,
                            result: load(&path, stream_from),
                        },
                        Request::Prepare {
                            ticket,
                            path,
                            offset,
                            looping,
                            buffer_ms,
                        } => Done::Prepared {
                            ticket,
                            result: prepare(&path, offset, looping, buffer_ms),
                        },
                    };
                    if outbox.send(answer).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            requests: thread.is_some().then_some(requests),
            done,
            thread,
        }
    }

    /// Asks for `request`; `false` without a loading thread (it could not start).
    pub(crate) fn send(&self, request: Request) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|tx| tx.send(request).is_ok())
    }

    /// The next answer, if one is ready.
    pub(crate) fn next(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // The thread ends with its inbox.
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads `path`: a WAV file, or a compressed one shorter than `stream_from` seconds, into
/// a clip; a longer compressed one is only measured (it streams when it plays).
fn load(path: &Path, stream_from: f32) -> Result<Loaded, String> {
    engine_profiler::zone!("app::sound::load");
    let why = |e: &dyn core::fmt::Display| format!("{}: {e}", path.display());
    let mut head = [0u8; 12];
    {
        use std::io::Read;
        let mut file = std::fs::File::open(path).map_err(|e| why(&e))?;
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..]).map_err(|e| why(&e))? {
                0 => break,
                n => filled += n,
            }
        }
    }
    if head.starts_with(b"RIFF") && &head[8..12] == b"WAVE" {
        let bytes = std::fs::read(path).map_err(|e| why(&e))?;
        return Clip::from_wav(&bytes)
            .map(Loaded::Clip)
            .map_err(|e| why(&e));
    }
    let mut decoder = engine_audio_decode::open_file(path).map_err(|e| why(&e))?;
    let rate = decoder.sample_rate();
    let short = decoder
        .frames()
        .is_some_and(|frames| (frames as f64) < f64::from(stream_from.max(0.0)) * f64::from(rate));
    if short {
        Clip::from_decoder(decoder.as_mut())
            .map(Loaded::Clip)
            .map_err(|e| why(&e))
    } else {
        Ok(Loaded::Long { rate })
    }
}

/// Opens `path` and makes it ready to play from `offset`: the seek and the first decode.
fn prepare(
    path: &Path,
    offset: u64,
    looping: bool,
    buffer_ms: u32,
) -> Result<PreparedStream, String> {
    engine_profiler::zone!("app::sound::prepare");
    let why = |e: &dyn core::fmt::Display| format!("{}: {e}", path.display());
    let decoder = engine_audio_decode::open_file(path).map_err(|e| why(&e))?;
    PreparedStream::new(decoder, offset, looping, buffer_ms).map_err(|e| why(&e))
}
