use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};

const MAX_TEXT: usize = 96 * 1024;

#[derive(Default)]
pub(super) struct Editor(Mutex<()>);

fn read(path: &Path) -> Result<(String, String)> {
    ensure!(fs::metadata(path)?.is_file(), "not a regular text file");
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_TEXT && !bytes.contains(&0),
        "file is binary or exceeds the text editor limit"
    );
    let content = String::from_utf8(bytes)?;
    let revision = format!("{:x}", Sha256::digest(content.as_bytes()));
    Ok((content, revision))
}

impl Editor {
    pub fn read(&self, path: &Path) -> Result<Value> {
        let (content, revision) = read(path)?;
        Ok(json!({"content":content, "revision":revision}))
    }

    pub fn save(&self, path: &Path, content: &str, expected: &str) -> Result<(String, Value)> {
        ensure!(
            content.len() <= MAX_TEXT && !content.as_bytes().contains(&0),
            "invalid editor text"
        );
        let _guard = self.0.lock().unwrap();
        let (before, revision) = read(path)?;
        ensure!(
            revision == expected,
            "REVISION_CONFLICT: reload the file before saving"
        );
        let mut staging = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        staging
            .as_file()
            .set_permissions(fs::metadata(path)?.permissions())?;
        staging.write_all(content.as_bytes())?;
        staging.as_file().sync_all()?;
        ensure!(
            read(path)?.1 == expected,
            "REVISION_CONFLICT: file changed during save"
        );
        staging.persist(path)?;
        let after = format!("{:x}", Sha256::digest(content.as_bytes()));
        let diff = similar::TextDiff::from_lines(before.as_str(), content);
        let diff_text = diff.unified_diff().context_radius(3).to_string();
        Ok((
            after.clone(),
            json!({"beforeRevision":revision,"afterRevision":after,"diff":diff_text,
                "additions": diff.iter_all_changes().filter(|c| c.tag()==similar::ChangeTag::Insert).count(),
                "deletions": diff.iter_all_changes().filter(|c| c.tag()==similar::ChangeTag::Delete).count()}),
        ))
    }
}
