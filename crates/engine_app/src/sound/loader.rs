//! Sounds read and decoded off the game thread (ADR 0105): a thread answers the sound
//! system's requests, so no file read, decode or seek ever holds up a frame. It reads the
//! sounds folder or the sound pack ([`Store`]).

use engine_assets::sounds::{SoundFormat, SoundPack};
use engine_audio::{Clip, Decoder, PreparedStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

/// Where the sounds' files are.
#[derive(Debug, Clone)]
pub(crate) enum Store {
    /// A folder of files, by their paths under it.
    Folder(PathBuf),
    /// A sound pack (`ofeliaengine sounds build`).
    Pack(Arc<SoundPack>),
}

impl Store {
    fn decoder(&self, path: &str) -> Result<Box<dyn Decoder>, String> {
        match self {
            Self::Folder(folder) => {
                let file = folder.join(path);
                engine_audio_decode::open_file(&file)
                    .map_err(|e| format!("{}: {e}", file.display()))
            }
            Self::Pack(pack) => {
                let why = |e: &dyn core::fmt::Display| format!("{path} in the sound pack: {e}");
                let sound = *pack
                    .get(path)
                    .ok_or_else(|| format!("{path}: not in the sound pack"))?;
                match pack.region(&sound).map_err(|e| why(&e))? {
                    // Compressed audio streams in place from the pack's file.
                    Some(region) => engine_audio_decode::open(region).map_err(|e| why(&e)),
                    None => {
                        let bytes = pack.read(&sound).map_err(|e| why(&e))?;
                        engine_audio_decode::open_bytes(bytes).map_err(|e| why(&e))
                    }
                }
            }
        }
    }
}

/// What a sound file turned out to be.
pub(crate) enum Loaded {
    /// Short (or a WAV file): decoded whole, played from memory.
    Clip(Clip),
    /// Long: streamed every time it plays.
    Long {
        /// Its sample rate in Hz.
        rate: u32,
    },
}

/// A request to the loading thread.
pub(crate) enum Request {
    /// Reads sound `id` at `path`: decoded whole when shorter than `stream_from` seconds.
    Load {
        id: u32,
        path: String,
        stream_from: f32,
    },
    /// Opens a long sound for one play from `offset` (frames at its rate).
    Prepare {
        ticket: u64,
        path: String,
        offset: u64,
        looping: bool,
        buffer_ms: u32,
    },
    /// File `path` (anywhere, not the store's) for the editor's preview from `from`
    /// seconds (E11-3b).
    Preview {
        ticket: u64,
        path: PathBuf,
        stream_from: f32,
        from: f64,
        looping: bool,
        buffer_ms: u32,
    },
}

/// A file made ready for the editor's preview.
pub(crate) enum PreviewSound {
    /// Short (or a WAV file): decoded whole.
    Clip(Clip),
    /// Long: opened as a stream where the preview starts; its rate and length in frames,
    /// when the file says.
    Stream {
        stream: PreparedStream,
        rate: u32,
        frames: Option<u64>,
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
    Preview {
        ticket: u64,
        result: Result<PreviewSound, String>,
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
    pub(crate) fn start(store: Store) -> Self {
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
                            result: load(&store, &path, stream_from),
                        },
                        Request::Prepare {
                            ticket,
                            path,
                            offset,
                            looping,
                            buffer_ms,
                        } => Done::Prepared {
                            ticket,
                            result: prepare(&store, &path, offset, looping, buffer_ms),
                        },
                        Request::Preview {
                            ticket,
                            path,
                            stream_from,
                            from,
                            looping,
                            buffer_ms,
                        } => Done::Preview {
                            ticket,
                            result: preview(&path, stream_from, from, looping, buffer_ms),
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
fn load(store: &Store, path: &str, stream_from: f32) -> Result<Loaded, String> {
    engine_profiler::zone!("app::sound::load");
    let short = |frames: Option<u64>, rate: u32| {
        frames.is_some_and(|f| (f as f64) < f64::from(stream_from.max(0.0)) * f64::from(rate))
    };
    match store {
        Store::Folder(folder) => {
            let file = folder.join(path);
            let why = |e: &dyn core::fmt::Display| format!("{}: {e}", file.display());
            let mut head = [0u8; 12];
            {
                use std::io::Read;
                let mut f = std::fs::File::open(&file).map_err(|e| why(&e))?;
                let mut filled = 0;
                while filled < head.len() {
                    match f.read(&mut head[filled..]).map_err(|e| why(&e))? {
                        0 => break,
                        n => filled += n,
                    }
                }
            }
            if SoundFormat::sniff(&head) == Some(SoundFormat::Wav) {
                let bytes = std::fs::read(&file).map_err(|e| why(&e))?;
                return Clip::from_wav(&bytes)
                    .map(Loaded::Clip)
                    .map_err(|e| why(&e));
            }
            let mut decoder = engine_audio_decode::open_file(&file).map_err(|e| why(&e))?;
            let rate = decoder.sample_rate();
            match decoder.frames() {
                Some(frames) if short(Some(frames), rate) => Clip::from_decoder(decoder.as_mut())
                    .map(Loaded::Clip)
                    .map_err(|e| why(&e)),
                Some(_) => Ok(Loaded::Long { rate }),
                // Ogg files do not say their length: decoded up to the threshold, a sound
                // that ends first is a clip.
                None => {
                    let limit = (f64::from(stream_from.max(0.0)) * f64::from(rate)) as u64;
                    match decode_up_to(decoder.as_mut(), limit).map_err(|e| why(&e))? {
                        Some(clip) => Ok(Loaded::Clip(clip)),
                        None => Ok(Loaded::Long { rate }),
                    }
                }
            }
        }
        Store::Pack(pack) => {
            let why = |e: &dyn core::fmt::Display| format!("{path} in the sound pack: {e}");
            let sound = *pack
                .get(path)
                .ok_or_else(|| format!("{path}: not in the sound pack"))?;
            if sound.format != SoundFormat::Wav && !short(sound.frames, sound.sample_rate) {
                return Ok(Loaded::Long {
                    rate: sound.sample_rate,
                });
            }
            let bytes = pack.read(&sound).map_err(|e| why(&e))?;
            if sound.format == SoundFormat::Wav {
                Clip::from_wav(&bytes)
                    .map(Loaded::Clip)
                    .map_err(|e| why(&e))
            } else {
                engine_audio_decode::decode(bytes)
                    .map(Loaded::Clip)
                    .map_err(|e| why(&e))
            }
        }
    }
}

/// The whole of `decoder` as a clip when it ends within `limit` frames; `None` when it
/// goes on past them.
fn decode_up_to(decoder: &mut dyn Decoder, limit: u64) -> Result<Option<Clip>, String> {
    let channels = usize::from(decoder.channels().max(1));
    let mut samples = Vec::new();
    let mut scratch = vec![0.0f32; 4096 * channels];
    let mut frames = 0u64;
    loop {
        let n = decoder.read(&mut scratch).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        frames += n as u64;
        if frames >= limit {
            return Ok(None);
        }
        samples.extend_from_slice(&scratch[..n * channels]);
    }
    Clip::from_interleaved(decoder.sample_rate(), decoder.channels(), &samples)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// File `path` (anywhere, not the store's) made ready for the editor's preview from
/// `from` seconds: decoded whole when short (or a WAV file), else opened as a stream
/// there.
fn preview(
    path: &Path,
    stream_from: f32,
    from: f64,
    looping: bool,
    buffer_ms: u32,
) -> Result<PreviewSound, String> {
    engine_profiler::zone!("app::sound::preview");
    // The folder's reading, by the file's own path.
    let disk = Store::Folder(PathBuf::new());
    let name = path.to_string_lossy();
    match load(&disk, &name, stream_from)? {
        Loaded::Clip(clip) => Ok(PreviewSound::Clip(clip)),
        Loaded::Long { rate } => {
            // An Ogg stream does not say its length: its last page does.
            let frames = engine_audio_decode::open_file(path)
                .ok()
                .and_then(|d| d.frames())
                .or_else(|| engine_audio_decode::ogg_last_granule(path));
            let offset = (from.max(0.0) * f64::from(rate)) as u64;
            let stream = prepare(&disk, &name, offset, looping, buffer_ms)?;
            Ok(PreviewSound::Stream {
                stream,
                rate,
                frames,
            })
        }
    }
}

/// Opens `path` and makes it ready to play from `offset`: the seek and the first decode.
fn prepare(
    store: &Store,
    path: &str,
    offset: u64,
    looping: bool,
    buffer_ms: u32,
) -> Result<PreparedStream, String> {
    engine_profiler::zone!("app::sound::prepare");
    let decoder = store.decoder(path)?;
    PreparedStream::new(decoder, offset, looping, buffer_ms).map_err(|e| format!("{path}: {e}"))
}
