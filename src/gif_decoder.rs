// SPDX-License-Identifier: MPL-2.0

//! Animated GIF decoder for live wallpaper backgrounds.
//!
//! Decodes all frames from a GIF file upfront (full pre-decode) for smooth
//! playback. Frame delays are capped at a minimum of ~33ms (30 FPS) to keep
//! CPU/GPU usage reasonable on mobile hardware.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

/// Minimum delay between frames (~30 FPS cap).
const MIN_FRAME_DELAY: Duration = Duration::from_millis(33);

/// A collection of pre-decoded GIF frames with per-frame delays.
#[derive(Debug)]
pub struct AnimatedFrames {
    frames: Vec<DecodedFrame>,
    current_index: usize,
}

/// A single decoded frame and the delay before the next frame.
#[derive(Debug)]
pub struct DecodedFrame {
    pub image: DynamicImage,
    pub delay: Duration,
}

impl AnimatedFrames {
    /// Attempt to decode an animated GIF from `path`.
    ///
    /// Returns `None` if the file is not a valid GIF or contains fewer than 2
    /// frames (i.e. it is a static image and should be handled by the normal
    /// wallpaper path).
    pub fn from_path(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let decoder = GifDecoder::new(reader).ok()?;
        let raw_frames = decoder.into_frames();

        let mut frames = Vec::new();
        for frame_result in raw_frames {
            let frame = match frame_result {
                Ok(f) => f,
                Err(why) => {
                    tracing::warn!(?why, "skipping malformed GIF frame");
                    continue;
                }
            };

            let delay = Duration::from(frame.delay()).max(MIN_FRAME_DELAY);
            let image = DynamicImage::ImageRgba8(frame.into_buffer());
            frames.push(DecodedFrame { image, delay });
        }

        // Treat single-frame (or empty) GIFs as static images.
        if frames.len() <= 1 {
            return None;
        }

        tracing::info!(
            frame_count = frames.len(),
            "loaded animated GIF with {} frames",
            frames.len()
        );

        Some(Self {
            frames,
            current_index: 0,
        })
    }

    /// Reference to the frame currently being displayed.
    pub fn current_frame(&self) -> &DecodedFrame {
        &self.frames[self.current_index]
    }

    /// Advance to the next frame (wraps around).
    pub fn advance(&mut self) {
        self.current_index = (self.current_index + 1) % self.frames.len();
    }

    /// Delay of the *current* frame (how long to show it before advancing).
    pub fn current_delay(&self) -> Duration {
        self.frames[self.current_index].delay
    }
}
