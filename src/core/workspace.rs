use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{ImageBuffer, ImageFormat, Rgba};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};

#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub path: PathBuf,
    pub name: String,
    pub dimensions: (u32, u32),
    pub size_kb: u64,
}

pub struct ImageWorkspace {
    _tmp_dir: TempDir,
    img_dir: PathBuf,
}

impl ImageWorkspace {
    pub fn new() -> Result<Self> {
        let tmp_dir = tempfile::Builder::new().prefix("archon-").tempdir()?;
        let img_dir = tmp_dir.path().join("images");
        fs::create_dir_all(&img_dir)?;
        Ok(Self {
            _tmp_dir: tmp_dir,
            img_dir,
        })
    }

    pub fn list_attachments(&self) -> Vec<ImageAttachment> {
        let mut paths: Vec<PathBuf> = fs::read_dir(&self.img_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("png"))
            .collect();

        paths.sort_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });

        paths
            .into_iter()
            .filter_map(|path| {
                let dimensions = image::image_dimensions(&path).ok()?;
                let size_kb = path.metadata().ok()?.len() / 1024;
                let name = path.file_name()?.to_string_lossy().to_string();
                Some(ImageAttachment {
                    path,
                    name,
                    dimensions,
                    size_kb: size_kb.max(1),
                })
            })
            .collect()
    }

    pub fn encode_image(path: &Path) -> Result<String> {
        let bytes = fs::read(path)?;
        Ok(STANDARD.encode(bytes))
    }

    pub fn paste_clipboard(&self) -> Result<PathBuf> {
        let out_path = self.gen_path("paste");

        // 1. Primary path: Native Wayland protocol (wlr-data-control / ext-data-control)
        // Ideal for Hyprland / CachyOS without requiring external binaries or XWayland.
        if let Ok(bytes) = Self::read_wayland_clipboard_image() {
            let img = image::load_from_memory(&bytes)
                .context("Failed to decode image from Wayland clipboard")?;
            img.save_with_format(&out_path, ImageFormat::Png)?;
            return Ok(out_path);
        }

        // 2. Fallback path: Arboard for X11 / other desktop environments
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if let Ok(img_data) = clipboard.get_image() {
                let width = img_data.width as u32;
                let height = img_data.height as u32;
                let raw = img_data.bytes.into_owned();

                if let Some(buffer) = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, raw) {
                    buffer.save_with_format(&out_path, ImageFormat::Png)?;
                    return Ok(out_path);
                }
            }
        }

        bail!("No valid image found in clipboard")
    }

    fn read_wayland_clipboard_image() -> Result<Vec<u8>> {
        for mime in ["image/png", "image/jpeg", "image/webp", "image/bmp"] {
            if let Ok((mut pipe, _)) =
                get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific(mime))
            {
                let mut buf = Vec::new();
                if pipe.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    return Ok(buf);
                }
            }
        }
        bail!("No supported image MIME type in Wayland clipboard")
    }

    pub fn import_file(&self, src: impl AsRef<Path>) -> Result<PathBuf> {
        let src = src.as_ref();
        let img =
            image::open(src).with_context(|| format!("Cannot open image: {}", src.display()))?;
        let out_path = self.gen_path("file");
        img.save_with_format(&out_path, ImageFormat::Png)?;
        Ok(out_path)
    }

    pub fn clear(&self) {
        if let Ok(entries) = fs::read_dir(&self.img_dir) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn gen_path(&self, prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        self.img_dir.join(format!("{prefix}_{ts}.png"))
    }
}
