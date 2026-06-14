// SPDX-License-Identifier: MPL-2.0

//! Video decoder for live wallpaper backgrounds.
//!
//! Uses FFmpeg (via `ffmpeg-next`) to decode video files frame-by-frame in a
//! background thread. Frames are sent through a bounded channel so the decoder
//! naturally paces itself to the consumption rate (~30 FPS).

use image::{DynamicImage, RgbaImage};
use std::fmt;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ffmpeg_next as ffmpeg;

/// Minimum frame delay (~30 FPS cap), matching the GIF decoder.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(33);

/// Supported video file extensions.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "webm", "mkv", "avi", "mov", "wmv", "flv", "m4v", "ts", "ogv",
];

/// Check whether a file path looks like a supported video format.
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| VIDEO_EXTENSIONS.iter().any(|v| ext.eq_ignore_ascii_case(v)))
        .unwrap_or(false)
}

/// A handle to a background video-decoding thread.
///
/// Frames are decoded in a separate thread and delivered through a bounded
/// channel.  When this struct is dropped the channel closes and the thread
/// exits automatically.
pub struct VideoPlayer {
    frame_rx: mpsc::Receiver<DynamicImage>,
    /// How long each frame should be displayed.
    pub frame_delay: Duration,
    /// The most recently received frame (kept for redraws).
    current_frame: Option<DynamicImage>,
}

impl fmt::Debug for VideoPlayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoPlayer")
            .field("frame_delay", &self.frame_delay)
            .field("has_frame", &self.current_frame.is_some())
            .finish()
    }
}

impl VideoPlayer {
    /// Try to open `path` as a video file and start decoding in the background.
    ///
    /// Returns `None` if the file cannot be opened or contains no video stream.
    pub fn from_path(path: &Path) -> Option<Self> {
        ffmpeg::init().ok()?;

        let path_owned = path.to_path_buf();

        // Probe the file to get stream info & frame rate, then hand everything
        // off to the background thread.
        let input = ffmpeg::format::input(&path_owned).ok()?;
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)?;
        let stream_index = stream.index();

        // Compute frame delay from the stream's average frame rate.
        let rate = stream.avg_frame_rate();
        let frame_delay = if rate.1 != 0 && rate.0 != 0 {
            let fps = rate.0 as f64 / rate.1 as f64;
            Duration::from_secs_f64(1.0 / fps).max(MIN_FRAME_DELAY)
        } else {
            MIN_FRAME_DELAY
        };

        // We intentionally drop the probed `input` — the decode thread opens
        // its own context so there are no lifetime / borrow issues.
        drop(input);

        // Bounded channel: decoder blocks after buffering 2 frames, naturally
        // pacing itself to the main thread's consumption rate.
        let (tx, rx) = mpsc::sync_channel::<DynamicImage>(2);

        thread::Builder::new()
            .name("cosmic-bg-video".into())
            .spawn(move || {
                if let Err(why) = decode_loop(&path_owned, stream_index, &tx) {
                    tracing::error!(?why, "video decode thread exited with error");
                }
            })
            .ok()?;

        tracing::info!(
            path = %path.display(),
            fps = format_args!("{:.1}", 1.0 / frame_delay.as_secs_f64()),
            "video wallpaper started"
        );

        Some(Self {
            frame_rx: rx,
            frame_delay,
            current_frame: None,
        })
    }

    /// Advance to the latest available frame (non-blocking).
    ///
    /// Returns `true` if a new frame was received.
    pub fn advance(&mut self) -> bool {
        // Drain channel to get the most recent frame (skip stale ones).
        let mut got_new = false;
        while let Ok(frame) = self.frame_rx.try_recv() {
            self.current_frame = Some(frame);
            got_new = true;
        }
        got_new
    }

    /// The current frame to render, if any.
    pub fn current_image(&self) -> Option<&DynamicImage> {
        self.current_frame.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Background decode loop
// ---------------------------------------------------------------------------

fn decode_loop(
    path: &Path,
    stream_index: usize,
    tx: &mpsc::SyncSender<DynamicImage>,
) -> Result<(), ffmpeg::Error> {
    loop {
        let mut input = ffmpeg::format::input(path)?;

        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;

        let codec_params = stream.parameters();
        let context = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
        let mut decoder = context.decoder().video()?;

        let mut scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            decoder.width(),
            decoder.height(),
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;

        let mut decoded_frame = ffmpeg::frame::Video::empty();
        let mut rgba_frame = ffmpeg::frame::Video::empty();

        // Read all packets from the stream.
        for (stream, packet) in input.packets() {
            if stream.index() != stream_index {
                continue;
            }

            if decoder.send_packet(&packet).is_err() {
                continue;
            }

            while decoder.receive_frame(&mut decoded_frame).is_ok() {
                if scaler.run(&decoded_frame, &mut rgba_frame).is_err() {
                    continue;
                }

                let img = rgba_frame_to_image(&rgba_frame);
                if tx.send(img).is_err() {
                    // Receiver dropped — wallpaper changed, exit thread.
                    return Ok(());
                }
            }
        }

        // Flush the decoder at EOF.
        decoder.send_eof()?;
        while decoder.receive_frame(&mut decoded_frame).is_ok() {
            if scaler.run(&decoded_frame, &mut rgba_frame).is_ok() {
                let img = rgba_frame_to_image(&rgba_frame);
                if tx.send(img).is_err() {
                    return Ok(());
                }
            }
        }

        // Loop: the outer `loop {}` re-opens the file and starts over.
        tracing::debug!("video reached EOF, looping");
    }
}

/// Convert an FFmpeg RGBA video frame into an `image::DynamicImage`.
fn rgba_frame_to_image(frame: &ffmpeg::frame::Video) -> DynamicImage {
    let width = frame.width();
    let height = frame.height();
    let stride = frame.stride(0);
    let data = frame.data(0);

    // If the stride matches width*4 we can use the data directly.
    if stride == width as usize * 4 {
        let buf = data[..width as usize * height as usize * 4].to_vec();
        DynamicImage::ImageRgba8(RgbaImage::from_raw(width, height, buf).unwrap())
    } else {
        // Copy row-by-row to remove padding.
        let mut buf = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height as usize {
            let row_start = y * stride;
            let row_end = row_start + width as usize * 4;
            buf.extend_from_slice(&data[row_start..row_end]);
        }
        DynamicImage::ImageRgba8(RgbaImage::from_raw(width, height, buf).unwrap())
    }
}
