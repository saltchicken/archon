use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageBuffer, ImageFormat, Rgba};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

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
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "png"))
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
                let size_kb = (path.metadata().ok()?.len() / 1024).max(1);
                let name = path.file_name()?.to_string_lossy().into_owned();
                Some(ImageAttachment {
                    path,
                    name,
                    dimensions,
                    size_kb,
                })
            })
            .collect()
    }

    pub fn encode_image(path: &Path) -> Result<String> {
        Ok(STANDARD.encode(fs::read(path)?))
    }

    pub fn paste_clipboard(&self) -> Result<PathBuf> {
        let out_path = self.gen_path("paste");

        if let Some(bytes) = Self::read_wayland_clipboard_image() {
            image::load_from_memory(&bytes)
                .context("Failed to decode image from Wayland clipboard")?
                .save_with_format(&out_path, ImageFormat::Png)?;
            return Ok(out_path);
        }

        if let Some(buffer) = arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_image().ok())
            .and_then(|img| {
                ImageBuffer::<Rgba<u8>, _>::from_raw(
                    img.width as u32,
                    img.height as u32,
                    img.bytes.into_owned(),
                )
            })
        {
            buffer.save_with_format(&out_path, ImageFormat::Png)?;
            return Ok(out_path);
        }

        bail!("No valid image found in clipboard")
    }

    fn read_wayland_clipboard_image() -> Option<Vec<u8>> {
        ["image/png", "image/jpeg", "image/webp", "image/bmp"]
            .into_iter()
            .find_map(|mime| {
                let (mut pipe, _) = get_contents(
                    ClipboardType::Regular,
                    Seat::Unspecified,
                    MimeType::Specific(mime),
                )
                .ok()?;
                let mut buf = Vec::new();
                (pipe.read_to_end(&mut buf).is_ok() && !buf.is_empty()).then_some(buf)
            })
    }

    pub fn import_file(&self, src: impl AsRef<Path>) -> Result<PathBuf> {
        let src = src.as_ref();
        let out_path = self.gen_path("file");
        image::open(src)
            .with_context(|| format!("Cannot open image: {}", src.display()))?
            .save_with_format(&out_path, ImageFormat::Png)?;
        Ok(out_path)
    }

    pub fn restore_from_session(&self, asset_paths: &[PathBuf]) -> Vec<PathBuf> {
        self.clear();
        asset_paths
            .iter()
            .enumerate()
            .filter(|(_, src)| src.exists())
            .filter_map(|(idx, src)| {
                let name = src
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map_or_else(|| format!("restored_{idx}.png"), String::from);
                let dest = self.img_dir.join(name);
                fs::copy(src, &dest).ok().map(|_| dest)
            })
            .collect()
    }

    pub fn clear(&self) {
        for entry in fs::read_dir(&self.img_dir).into_iter().flatten().flatten() {
            let _ = fs::remove_file(entry.path());
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
