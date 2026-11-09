use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::GenericImageView;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

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
        Ok(Self { _tmp_dir: tmp_dir, img_dir })
    }

    pub fn list_attachments(&self) -> Vec<ImageAttachment> {
        let mut paths: Vec<PathBuf> = fs::read_dir(&self.img_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("png"))
            .collect();

        paths.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH));

        paths
            .into_iter()
            .filter_map(|path| {
                let img = image::open(&path).ok()?;
                let size_kb = path.metadata().ok()?.len() / 1024;
                let name = path.file_name()?.to_string_lossy().to_string();
                Some(ImageAttachment {
                    path,
                    name,
                    dimensions: img.dimensions(),
                    size_kb: size_kb.max(1),
                })
            })
            .collect()
    }

    pub fn encode_image(path: &Path) -> Result<String> {
        let bytes = fs::read(path)?;
        Ok(STANDARD.encode(bytes))
    }

    pub fn capture_screen_region(&self) -> Result<PathBuf> {
        let out_path = self.gen_path("shot");
        let slurp = Command::new("slurp").output().context("Failed to run slurp")?;
        if !slurp.status.success() {
            bail!("Capture cancelled");
        }
        let geom = String::from_utf8_lossy(&slurp.stdout).trim().to_string();
        if geom.is_empty() {
            bail!("Empty region selected");
        }

        let grim = Command::new("grim").arg("-g").arg(&geom).arg(&out_path).status()?;
        if !grim.success() || !Self::validate_file(&out_path) {
            bail!("grim failed to capture region");
        }
        Ok(out_path)
    }

    pub fn paste_clipboard(&self) -> Result<PathBuf> {
        let out_path = self.gen_path("paste");
        let file = fs::File::create(&out_path)?;
        let status = Command::new("wl-paste")
            .args(["-t", "image/png"])
            .stdout(Stdio::from(file))
            .stderr(Stdio::null())
            .status()?;

        if !status.success() || !Self::validate_file(&out_path) {
            let _ = fs::remove_file(&out_path);
            bail!("No image found in Wayland clipboard");
        }
        Ok(out_path)
    }

    pub fn import_file(&self, src: impl AsRef<Path>) -> Result<PathBuf> {
        let src = src.as_ref();
        let img = image::open(src).with_context(|| format!("Cannot open image: {}", src.display()))?;
        let out_path = self.gen_path("file");
        img.save_with_format(&out_path, image::ImageFormat::Png)?;
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
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        self.img_dir.join(format!("{prefix}_{ts}.png"))
    }

    fn validate_file(path: &Path) -> bool {
        path.metadata().map(|m| m.len() > 0).unwrap_or(false)
    }
}
