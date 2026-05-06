use crate::document::pdf_text_extractor::PdfTextExtractor;
use std::path::Path;

pub struct PdfProcessor {
    extractor: PdfTextExtractor,
}

impl PdfProcessor {
    pub fn new() -> Self {
        Self {
            extractor: PdfTextExtractor,
        }
    }

    pub fn process(
        &self,
        pdf_path: impl AsRef<Path>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let path = pdf_path.as_ref();
        self.extractor.extract_markdown(path)
    }
}

impl Default for PdfProcessor {
    fn default() -> Self {
        Self::new()
    }
}
