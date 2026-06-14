// SPDX-License-Identifier: MPL-2.0

use crate::gif_decoder::AnimatedFrames;
use crate::video_decoder::VideoPlayer;
use crate::{CosmicBg, CosmicBgLayer};

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cosmic_bg_config::state::State;
use cosmic_bg_config::{Color, Entry, SamplingMethod, ScalingMode, Source};
use cosmic_config::CosmicConfigEntry;
use image::{DynamicImage, ImageReader, Limits};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rand::rng;
use rand::seq::SliceRandom;
use sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use sctk::reexports::calloop::{self, RegistrationToken};
use sctk::reexports::client::QueueHandle;
use tracing::error;
use walkdir::WalkDir;

// TODO filter images by whether they seem to match dark / light mode
// Alternatively only load from light / dark subdirectories given a directory source when this is active

#[derive(Debug)]
pub struct Wallpaper {
    pub entry: Entry,
    pub layers: Vec<CosmicBgLayer>,
    pub image_queue: VecDeque<PathBuf>,
    loop_handle: calloop::LoopHandle<'static, CosmicBg>,
    queue_handle: QueueHandle<CosmicBg>,
    current_source: Option<Source>,
    // Cache of source image, if `current_source` is a `Source::Path`
    current_image: Option<image::DynamicImage>,
    timer_token: Option<RegistrationToken>,
    /// Pre-decoded animated GIF frames (if current source is an animated GIF).
    animated_frames: Option<AnimatedFrames>,
    /// Background-thread video decoder (if current source is a video file).
    video_player: Option<VideoPlayer>,
    /// calloop token for the per-frame animation timer (shared by GIF & video).
    animation_token: Option<RegistrationToken>,
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        if let Some(token) = self.timer_token.take() {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.animation_token.take() {
            self.loop_handle.remove(token);
        }
    }
}

impl Wallpaper {
    pub fn new(
        entry: Entry,
        queue_handle: QueueHandle<CosmicBg>,
        loop_handle: calloop::LoopHandle<'static, CosmicBg>,
        source_tx: calloop::channel::SyncSender<(String, notify::Event)>,
    ) -> Self {
        let mut wallpaper = Wallpaper {
            entry,
            layers: Vec::new(),
            current_source: None,
            current_image: None,
            animated_frames: None,
            video_player: None,
            image_queue: VecDeque::default(),
            timer_token: None,
            animation_token: None,
            loop_handle,
            queue_handle,
        };

        wallpaper.load_images();
        wallpaper.try_load_animated();
        wallpaper.register_timer();
        wallpaper.watch_source(source_tx);
        wallpaper
    }

    pub fn save_state(&self) -> Result<(), cosmic_config::Error> {
        let Some(cur_source) = self.current_source.clone() else {
            return Ok(());
        };
        let state_helper = State::state()?;
        let mut state = State::get_entry(&state_helper).unwrap_or_default();
        for l in &self.layers {
            let name = l.output_info.name.clone().unwrap_or_default();
            if let Some((_, source)) = state
                .wallpapers
                .iter_mut()
                .find(|(output, _)| *output == name)
            {
                *source = cur_source.clone();
            } else {
                state.wallpapers.push((name, cur_source.clone()))
            }
        }
        state.write_entry(&state_helper)
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self) {
        let start = Instant::now();
        let mut cur_resized_img: Option<DynamicImage> = None;

        for layer in self.layers.iter_mut().filter(|layer| layer.needs_redraw) {
            let Some(pool) = layer.pool.as_mut() else {
                continue;
            };

            let Some(fractional_scale) = layer.fractional_scale else {
                continue;
            };

            let Some((width, height)) = layer.size else {
                continue;
            };

            let width = width * fractional_scale / 120;
            let height = height * fractional_scale / 120;

            if cur_resized_img
                .as_ref()
                .is_none_or(|img| img.width() != width || img.height() != height)
            {
                // Animated GIF: use the current pre-decoded frame as image source.
                if let Some(ref animated) = self.animated_frames {
                    let img = &animated.current_frame().image;
                    cur_resized_img = match self.entry.scaling_mode {
                        ScalingMode::Fit(ref color) => {
                            Some(crate::scaler::fit(img, color, width, height))
                        }
                        ScalingMode::Zoom => Some(crate::scaler::zoom(img, width, height)),
                        ScalingMode::Stretch => {
                            Some(crate::scaler::stretch(img, width, height))
                        }
                    };
                // Video: use the latest frame from the background decoder thread.
                } else if let Some(ref video) = self.video_player {
                    if let Some(img) = video.current_image() {
                        cur_resized_img = match self.entry.scaling_mode {
                            ScalingMode::Fit(ref color) => {
                                Some(crate::scaler::fit(img, color, width, height))
                            }
                            ScalingMode::Zoom => Some(crate::scaler::zoom(img, width, height)),
                            ScalingMode::Stretch => {
                                Some(crate::scaler::stretch(img, width, height))
                            }
                        };
                    }
                } else {
                    // Static image / color fallback (original logic).
                    let Some(source) = self.current_source.as_ref() else {
                        tracing::info!("No source for wallpaper");
                        continue;
                    };

                    cur_resized_img = match source {
                        Source::Path(path) => {
                            if self.current_image.is_none() {
                                self.current_image = match ImageReader::open(path)
                                    .ok()
                                    .and_then(|f| f.with_guessed_format().ok())
                                {
                                    Some(mut f) => {
                                        let mut limits = Limits::default();
                                        limits.max_alloc = Some(1024 * 1024 * 1024);
                                        f.limits(limits);

                                        match f.decode() {
                                            Ok(img) => Some(img),
                                            Err(why) => {
                                                tracing::warn!(
                                                    ?why,
                                                    "Failed to decode image: {}",
                                                    path.display()
                                                );
                                                continue;
                                            }
                                        }
                                    }
                                    None => continue,
                                };
                            }
                            let img = self.current_image.as_ref().unwrap();

                            match self.entry.scaling_mode {
                                ScalingMode::Fit(color) => {
                                    Some(crate::scaler::fit(img, &color, width, height))
                                }

                                ScalingMode::Zoom => {
                                    Some(crate::scaler::zoom(img, width, height))
                                }

                                ScalingMode::Stretch => {
                                    Some(crate::scaler::stretch(img, width, height))
                                }
                            }
                        }

                        Source::Color(Color::Single([r, g, b])) => {
                            Some(image::DynamicImage::from(
                                crate::colored::single([*r, *g, *b], width, height),
                            ))
                        }

                        Source::Color(Color::Gradient(gradient)) => {
                            match crate::colored::gradient(gradient, width, height) {
                                Ok(buffer) => Some(image::DynamicImage::from(buffer)),
                                Err(why) => {
                                    tracing::error!(
                                        ?gradient,
                                        ?why,
                                        "color gradient in config is invalid"
                                    );
                                    None
                                }
                            }
                        }
                    };
                }
            }

            let image = cur_resized_img.as_ref().unwrap();
            let buffer_result =
                crate::draw::canvas(pool, image, width as i32, height as i32, width as i32 * 4);

            match buffer_result {
                Ok(buffer) => {
                    crate::draw::layer_surface(
                        layer,
                        &self.queue_handle,
                        &buffer,
                        (width as i32, height as i32),
                    );
                    layer.needs_redraw = false;

                    let elapsed = Instant::now().duration_since(start);

                    tracing::debug!(?elapsed, source = ?self.entry.source, "wallpaper draw");
                }

                Err(why) => {
                    tracing::error!(?why, "wallpaper could not be drawn");
                }
            }
        }
    }

    pub fn load_images(&mut self) {
        let mut image_queue = VecDeque::new();
        let xdg_data_dirs: Vec<String> = match std::env::var("XDG_DATA_DIRS") {
            Ok(raw_xdg_data_dirs) => raw_xdg_data_dirs
                .split(':')
                .map(|s| format!("{}/backgrounds/", s))
                .collect(),
            Err(_) => Vec::new(),
        };

        match self.entry.source {
            Source::Path(ref source) => {
                tracing::debug!(?source, "loading images");

                if let Ok(source) = source.canonicalize() {
                    if source.is_dir() {
                        if xdg_data_dirs
                            .iter()
                            .any(|xdg_data_dir| source.starts_with(xdg_data_dir))
                        {
                            // Store paths of wallpapers to be used for the slideshow.
                            for img_path in WalkDir::new(source)
                                .follow_links(true)
                                .into_iter()
                                .filter_map(Result::ok)
                                .filter(|p| p.path().is_file())
                            {
                                image_queue.push_front(img_path.path().into());
                            }
                        } else if let Ok(dir) = source.read_dir() {
                            for entry in dir.filter_map(Result::ok) {
                                let Ok(path) = entry.path().canonicalize() else {
                                    continue;
                                };

                                if path.is_file() {
                                    image_queue.push_front(path);
                                }
                            }
                        }
                    } else if source.is_file() {
                        image_queue.push_front(source);
                    }
                }

                if image_queue.len() > 1 {
                    let image_slice = image_queue.make_contiguous();
                    match self.entry.sampling_method {
                        SamplingMethod::Alphanumeric => {
                            image_slice
                                .sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
                        }
                        SamplingMethod::Random => image_slice.shuffle(&mut rng()),
                    };

                    // If a wallpaper from this slideshow was previously set, resume with that wallpaper.
                    if let Some(Source::Path(last_path)) = current_image(&self.entry.output)
                        && image_queue.contains(&last_path)
                    {
                        while let Some(path) = image_queue.pop_front() {
                            if path == last_path {
                                image_queue.push_front(path);
                                break;
                            }

                            image_queue.push_back(path);
                        }
                    }
                }

                if let Some(current_image_path) = image_queue.pop_front() {
                    self.current_source = Some(Source::Path(current_image_path.clone()));
                    image_queue.push_back(current_image_path);
                }
            }

            Source::Color(ref c) => {
                self.current_source = Some(Source::Color(c.clone()));
            }
        };
        if let Err(err) = self.save_state() {
            error!("{err}");
        }
        self.image_queue = image_queue;
    }

    fn watch_source(&self, tx: calloop::channel::SyncSender<(String, notify::Event)>) {
        let Source::Path(ref source) = self.entry.source else {
            return;
        };

        let output = self.entry.output.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                if let Ok(e) = res {
                    let _ = tx.send((output.clone(), e));
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(_) => return,
        };

        tracing::debug!(output = self.entry.output, "watching source");

        if let Ok(m) = fs::metadata(source) {
            if m.is_dir() {
                let _ = watcher.watch(source, RecursiveMode::Recursive);
            } else if m.is_file() {
                let _ = watcher.watch(source, RecursiveMode::NonRecursive);
            }
        }
    }

    fn register_timer(&mut self) {
        let rotation_freq = self.entry.rotation_frequency;
        let cosmic_bg_clone = self.entry.output.clone();
        // set timer for rotation
        if rotation_freq > 0 {
            self.timer_token = self
                .loop_handle
                .insert_source(
                    Timer::from_duration(Duration::from_secs(rotation_freq)),
                    move |_, _, state: &mut CosmicBg| {
                        let span = tracing::debug_span!("Wallpaper::timer");
                        let _handle = span.enter();

                        let Some(item) = state
                            .wallpapers
                            .iter_mut()
                            .find(|w| w.entry.output == cosmic_bg_clone)
                        else {
                            return TimeoutAction::Drop; // Drop if no item found for this timer
                        };

                        if let Some(next) = item.image_queue.pop_front() {
                            item.current_source = Some(Source::Path(next.clone()));
                            if let Err(err) = item.save_state() {
                                error!("{err}");
                            }

                            item.image_queue.push_back(next);
                            item.clear_image();
                            item.try_load_animated();
                            item.draw();

                            return TimeoutAction::ToDuration(Duration::from_secs(rotation_freq));
                        }

                        TimeoutAction::Drop
                    },
                )
                .ok();
        }
    }

    fn clear_image(&mut self) {
        self.current_image = None;
        self.animated_frames = None;
        self.video_player = None; // drops thread via channel close
        if let Some(token) = self.animation_token.take() {
            self.loop_handle.remove(token);
        }
        for l in &mut self.layers {
            l.needs_redraw = true;
        }
    }

    /// Auto-detect animated source (GIF or video) from `current_source`.
    /// If animated, populates the appropriate field and registers the
    /// animation timer.
    fn try_load_animated(&mut self) {
        let Some(Source::Path(ref path)) = self.current_source else {
            return;
        };

        // 1) Try animated GIF first.
        let is_gif = path
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("gif"))
            .unwrap_or(false);

        if is_gif {
            match AnimatedFrames::from_path(path) {
                Some(frames) => {
                    tracing::info!(
                        path = %path.display(),
                        "animated GIF detected, starting live background"
                    );
                    self.animated_frames = Some(frames);
                    self.register_animation_timer();
                    return;
                }
                None => {
                    tracing::debug!(
                        path = %path.display(),
                        "GIF has ≤1 frame, treating as static image"
                    );
                    return;
                }
            }
        }

        // 2) Try video file.
        if crate::video_decoder::is_video_file(path) {
            match VideoPlayer::from_path(path) {
                Some(player) => {
                    tracing::info!(
                        path = %path.display(),
                        "video detected, starting live background"
                    );
                    self.video_player = Some(player);
                    self.register_animation_timer();
                }
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to open video, treating as static image"
                    );
                }
            }
        }
    }

    /// Register a calloop timer that drives GIF frame advancement or video
    /// frame consumption and triggers redraws at ~30 FPS.
    fn register_animation_timer(&mut self) {
        // Determine initial delay from whichever animated source is active.
        let delay = if let Some(ref animated) = self.animated_frames {
            animated.current_delay()
        } else if let Some(ref video) = self.video_player {
            video.frame_delay
        } else {
            return;
        };

        let output = self.entry.output.clone();

        self.animation_token = self
            .loop_handle
            .insert_source(
                Timer::from_duration(delay),
                move |_, _, state: &mut CosmicBg| {
                    let Some(wallpaper) = state
                        .wallpapers
                        .iter_mut()
                        .find(|w| w.entry.output == output)
                    else {
                        return TimeoutAction::Drop;
                    };

                    // --- GIF path ---
                    if wallpaper.animated_frames.is_some() {
                        let next_delay = {
                            let animated = wallpaper.animated_frames.as_mut().unwrap();
                            animated.advance();
                            animated.current_delay()
                        };

                        for l in &mut wallpaper.layers {
                            l.needs_redraw = true;
                        }
                        wallpaper.draw();

                        return TimeoutAction::ToDuration(next_delay);
                    }

                    // --- Video path ---
                    if wallpaper.video_player.is_some() {
                        let delay = {
                            let video = wallpaper.video_player.as_mut().unwrap();
                            video.advance();
                            video.frame_delay
                        };

                        for l in &mut wallpaper.layers {
                            l.needs_redraw = true;
                        }
                        wallpaper.draw();

                        return TimeoutAction::ToDuration(delay);
                    }

                    TimeoutAction::Drop
                },
            )
            .ok();
    }
}

fn current_image(output: &str) -> Option<Source> {
    let state = State::state().ok()?;
    let mut wallpapers = State::get_entry(&state)
        .unwrap_or_default()
        .wallpapers
        .into_iter();

    let wallpaper = if output == "all" {
        wallpapers.next()
    } else {
        wallpapers.into_iter().find(|(name, _path)| name == output)
    };

    wallpaper.map(|(_name, path)| path)
}
