//! Small previews of pictures for the file list. Decoding happens in
//! background threads; the list shows the normal icon until one is ready.

use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, SystemTime};

/// Longer side of a preview in pixels.
const SIZE: u32 = 96;
const MAX_CACHED: usize = 1500;
const MAX_QUEUED: usize = 300;
/// Bigger files are not decoded (a preview is not worth minutes of work).
const MAX_FILE: u64 = 200 << 20;

type Key = (PathBuf, Option<SystemTime>);

enum State {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

#[derive(Default)]
struct Inner {
    cache: HashMap<Key, State>,
    /// Newest request last; it is served first (that is what is on screen).
    queue: Vec<Key>,
    workers: bool,
}

#[derive(Default)]
pub struct Thumbs {
    shared: Arc<(Mutex<Inner>, Condvar)>,
}

pub fn is_picture(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp")
}

fn decode(path: &Path) -> Option<egui::ColorImage> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    let small = img.thumbnail(SIZE, SIZE).to_rgba8();
    let size = [small.width() as usize, small.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, small.as_raw()))
}

fn worker(shared: Weak<(Mutex<Inner>, Condvar)>, ctx: egui::Context) {
    loop {
        // The app (and its cache) may be gone: then stop.
        let Some(strong) = shared.upgrade() else { return };
        let (lock, cv) = &*strong;
        let key = {
            let mut inner = lock.lock().unwrap();
            match inner.queue.pop() {
                Some(k) => k,
                None => {
                    let _ = cv.wait_timeout(inner, Duration::from_secs(1)).unwrap();
                    continue;
                }
            }
        };
        let state = match decode(&key.0) {
            Some(img) => State::Ready(ctx.load_texture(format!("thumb:{}", key.0.display()), img, egui::TextureOptions::LINEAR)),
            None => State::Failed,
        };
        let mut inner = lock.lock().unwrap();
        if inner.cache.len() > MAX_CACHED {
            // Simple and good enough: start over, what is visible comes again.
            let waiting: Vec<Key> = inner.queue.clone();
            inner.cache.clear();
            for k in waiting {
                inner.cache.insert(k, State::Loading);
            }
        }
        inner.cache.insert(key, state);
        drop(inner);
        ctx.request_repaint();
    }
}

impl Thumbs {
    /// The preview of `path`, once it is ready (asks for it otherwise).
    pub fn get(&self, ctx: &egui::Context, path: &Path, mtime: Option<SystemTime>) -> Option<egui::TextureHandle> {
        let (lock, cv) = &*self.shared;
        let mut inner = lock.lock().unwrap();
        let key = (path.to_path_buf(), mtime);
        match inner.cache.get(&key) {
            Some(State::Ready(t)) => return Some(t.clone()),
            Some(_) => return None,
            None => {}
        }
        if !inner.workers {
            inner.workers = true;
            for _ in 0..2 {
                let (weak, ctx) = (Arc::downgrade(&self.shared), ctx.clone());
                std::thread::spawn(move || worker(weak, ctx));
            }
        }
        if inner.queue.len() >= MAX_QUEUED {
            // Scrolled past long ago: forget it (asked for again when visible).
            let old = inner.queue.remove(0);
            inner.cache.remove(&old);
        }
        inner.cache.insert(key.clone(), State::Loading);
        inner.queue.push(key);
        cv.notify_one();
        None
    }
}
