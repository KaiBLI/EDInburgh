use cpal::traits::HostTrait;
use derive_more::Debug;
use faad2::{version, Decoder as FaadDecoder};
// Added Frame and fixed the aliases here
use minimp3::{Decoder as Mp3Decoder, Frame, Error as Mp3Error}; 
use rodio::{buffer::SamplesBuffer, OutputStream, OutputStreamBuilder, Sink};
use shared::dab::msc::{AacpResult, AudioFormat};
use std::io::{Cursor, Error};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum AudioEvent {
    LevelsUpdated(AudioLevels),
}

#[derive(Debug, Clone)]
pub struct AudioLevels {
    pub peak: (f32, f32),
    pub peak_clamped: (f32, f32),
    pub rms: (f32, f32),
    pub peak_smooth: (f32, f32),
    pub rms_smooth: (f32, f32),
    #[debug(skip)]
    last_update: Instant,
}

impl AudioLevels {
    pub fn new() -> Self {
        Self {
            peak: (0.0, 0.0),
            peak_clamped: (0.0, 0.0),
            rms: (0.0, 0.0),
            peak_smooth: (0.0, 0.0),
            rms_smooth: (0.0, 0.0),
            last_update: Instant::now(),
        }
    }

    pub fn smooth(current: (f32, f32), previous: (f32, f32), dt: f32, decay_per_second: f32) -> (f32, f32) {
        fn smooth_channel(new: f32, prev: f32, dt: f32, decay: f32) -> f32 {
            if new > prev { new } else { (prev - decay * dt).max(new) }
        }
        (
            smooth_channel(current.0, previous.0, dt, decay_per_second),
            smooth_channel(current.1, previous.1, dt, decay_per_second),
        )
    }

    pub fn feed(&mut self, channels: usize, samples: &[f32]) {
        if samples.is_empty() { return; }
        let count = samples.len() / channels;
        let top_n = 64;
        let mut sum_l = 0.0;
        let mut sum_r = 0.0;
        let mut peaks_l = Vec::with_capacity(count);
        let mut peaks_r = Vec::with_capacity(count);

        for (i, sample) in samples.iter().enumerate() {
            let abs = sample.abs();
            if i % channels == 0 {
                peaks_l.push(abs);
                sum_l += sample * sample;
            } else {
                peaks_r.push(abs);
                sum_r += sample * sample;
            }
        }

        peaks_l.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        peaks_r.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

        let peak_l = peaks_l.iter().take(top_n).copied().sum::<f32>() / peaks_l.len().min(top_n) as f32;
        let peak_r = peaks_r.iter().take(top_n).copied().sum::<f32>() / peaks_r.len().min(top_n) as f32;

        self.peak = (peak_l, peak_r);
        self.peak_clamped = (peak_l.clamp(0.0, 1.0), peak_r.clamp(0.0, 1.0));
        self.rms = ((sum_l / count as f32).sqrt(), (sum_r / count as f32).sqrt());

        let now = Instant::now();
        let dt = now.duration_since(self.last_update).as_secs_f32();
        self.last_update = now;
        self.peak_smooth = Self::smooth(self.peak, self.peak_smooth, dt, 0.05);
        self.rms_smooth = Self::smooth(self.rms, self.rms_smooth, dt, 0.1);
    }
}

pub struct DecodedFrame {
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

#[derive(Debug)]
pub enum CodecType {
    Aac(#[debug(skip)] FaadDecoder),
    Mpeg2,
}

impl CodecType {
    fn decode(&mut self, data: &[u8]) -> Result<DecodedFrame, Error> {
        match self {
            CodecType::Aac(decoder) => {
                decoder.decode(data).map(|r| DecodedFrame {
                    channels: r.channels as u16,
                    sample_rate: r.sample_rate as u32,
                    samples: r.samples.to_vec(),
                }).map_err(|e| Error::other(e.to_string()))
            },
            CodecType::Mpeg2 => {
                let mut frame_decoder = Mp3Decoder::new(Cursor::new(data));
                match frame_decoder.next_frame() {
                    Ok(Frame { data, sample_rate, channels, .. }) => {
                        let samples_f32: Vec<f32> = data.iter()
                            .map(|&s| s as f32 / 32768.0)
                            .collect();
                        Ok(DecodedFrame {
                            channels: channels as u16,
                            sample_rate: sample_rate as u32,
                            samples: samples_f32,
                        })
                    },
                    Err(Mp3Error::Eof) => Err(Error::other("MP2 EOF")),
                    Err(e) => Err(Error::other(format!("MP2 Decode Error: {:?}", e))),
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct AudioDecoder {
    scid: u8,
    asc: Vec<u8>,
    audio_format: AudioFormat,
    codec: CodecType,
    #[debug(skip)]
    _stream: OutputStream,
    #[debug(skip)]
    sink: Arc<Mutex<Sink>>,
    #[debug(skip)]
    tx: UnboundedSender<AudioEvent>,
    levels: AudioLevels,
}

// ALL THE METHODS BELOW MUST BE INSIDE THIS BLOCK
impl AudioDecoder {
    #[allow(dead_code)]
    pub fn version() -> &'static str {
        version().0
    }

    fn create_codec(format: &AudioFormat) -> Result<CodecType, Error> {
        if !format.asc.is_empty() {
            let decoder = FaadDecoder::new(&format.asc).map_err(|_| Error::other("AAC Init fail"))?;
            Ok(CodecType::Aac(decoder))
        } else {
            Ok(CodecType::Mpeg2)
        }
    }

    pub fn new(scid: u8, use_jack: bool, initial_audio_format: AudioFormat, tx: UnboundedSender<AudioEvent>) -> Self {
        let codec = Self::create_codec(&initial_audio_format).expect("Decoder init failed");
        
        let host = {
            let mut selected_host = cpal::default_host();

            #[cfg(all(feature = "jack", target_os = "linux"))]
            if use_jack {
                // We search available hosts for JACK to avoid the enum variant error
                if let Some(jack_host) = cpal::available_hosts()
                    .into_iter()
                    .find(|id| id.name().to_lowercase().contains("jack")) 
                {
                    selected_host = cpal::host_from_id(jack_host).expect("Failed to init JACK host");
                }
            }
            selected_host
        };

        let device = host.default_output_device().expect("No audio device");
        let stream_handle = OutputStreamBuilder::from_device(device)
            .and_then(|x| x.open_stream())
            .expect("Stream error");
        
        let sink = Arc::new(Mutex::new(Sink::connect_new(stream_handle.mixer())));

        Self {
            scid,
            asc: initial_audio_format.asc.clone(),
            audio_format: initial_audio_format,
            codec,
            _stream: stream_handle,
            sink,
            tx,
            levels: AudioLevels::new(),
        }
    }

    fn reconfigure(&mut self, new_audio_format: &AudioFormat) -> Result<(), Error> {
        let new_codec = Self::create_codec(new_audio_format)?;
        self.codec = new_codec;
        self.audio_format = new_audio_format.clone();
        self.asc = new_audio_format.asc.clone();
        self.sink.lock().unwrap().stop();
        Ok(())
    }

    pub fn feed(&mut self, aac_result: &AacpResult) {
        if let Some(new_format) = &aac_result.audio_format {
            if new_format != &self.audio_format {
                let _ = self.reconfigure(new_format);
            }
        }

        if aac_result.scid != self.scid {
            self.sink.lock().unwrap().set_volume(0.0);
            let sink_clone = Arc::clone(&self.sink);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                for i in 1..=20 {
                    thread::sleep(Duration::from_millis(10));
                    if let Ok(s) = sink_clone.lock() { s.set_volume(i as f32 * 0.05); }
                }
            });
            self.levels = AudioLevels::new();
            self.scid = aac_result.scid;
        }

        for frame in &aac_result.frames {
            self.feed_au(frame);
        }
    }

    pub fn feed_au(&mut self, au_data: &[u8]) {
        if let Ok(r) = self.codec.decode(au_data) {
            self.levels.feed(r.channels as usize, &r.samples);
            
            // Fixed the ownership error by removing &
            self.sink.lock().unwrap().append(SamplesBuffer::new(
                r.channels,
                r.sample_rate,
                r.samples, 
            ));

            let _ = self.tx.send(AudioEvent::LevelsUpdated(self.levels.clone()));
        }
    }
}

unsafe impl Send for AudioDecoder {}