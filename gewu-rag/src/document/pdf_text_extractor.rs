use std::path::Path;
use std::process::Command;

pub struct PdfTextExtractor;

impl PdfTextExtractor {
    pub fn new() -> Self {
        Self
    }

    pub fn extract_markdown(&self, pdf_path: impl AsRef<Path>) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("pdftotext")
            .args(["-layout", pdf_path.as_ref().to_str().unwrap(), "-"])
            .output()?;

        if !output.status.success() {
            return Err(format!("pdftotext failed: {}", String::from_utf8_lossy(&output.stderr)).into());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

impl Default for PdfTextExtractor {
    fn default() -> Self {
        Self::new()
    }
}