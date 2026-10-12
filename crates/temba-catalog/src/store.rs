//! Durable, non-secret API definitions. One writer; commit before publishing in RAM.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use temba_core::{Catalog, Server};

pub fn load(path: &Path, baseline: &Catalog, allow_http: bool) -> Result<BTreeMap<String, Server>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).context("Cannot inspect persisted catalog"),
    };
    ensure!(
        metadata.len() <= 1024 * 1024,
        "Persisted catalog exceeds size limit"
    );
    let mut catalog: Catalog =
        serde_json::from_slice(&fs::read(path)?).context("Invalid persisted catalog")?;
    catalog.validate(allow_http)?;
    ensure!(
        baseline.servers.len() + catalog.servers.len() <= 256,
        "Combined catalog exceeds 256 servers"
    );
    for server in &catalog.servers {
        ensure!(
            !baseline.servers.iter().any(|s| s.id == server.id),
            "Persisted definition conflicts with Helm definition: {}",
            server.id
        );
    }
    Ok(catalog
        .servers
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect())
}

pub async fn persist(path: &Path, servers: &BTreeMap<String, Server>) -> Result<()> {
    let data = serde_json::to_vec(&Catalog {
        schema_version: 2,
        revision: String::new(),
        servers: servers.values().cloned().collect(),
    })?;
    ensure!(
        data.len() <= 1024 * 1024,
        "Persisted catalog exceeds size limit"
    );
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || atomic_write(&path, &data)).await?
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing catalog parent directory")?;
    // The PVC mount is the only writable directory. It must already exist.
    let directory = fs::File::open(parent)?;
    let temporary: PathBuf = path.with_extension("json.tmp");
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub fn test_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "temba-store-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path.join("additions.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn persists_reloads_deletes_and_rejects_corruption_and_collisions() {
        let path = test_path();
        let baseline = Catalog {
            schema_version: 2,
            revision: String::new(),
            servers: vec![],
        };
        assert!(load(&path, &baseline, false).unwrap().is_empty());
        let server: Server =
            serde_json::from_value(json!({"id":"added","endpoint":"https://example.com/mcp"}))
                .unwrap();
        let servers = BTreeMap::from([("added".into(), server.clone())]);
        persist(&path, &servers).await.unwrap();
        assert_eq!(load(&path, &baseline, false).unwrap(), servers);
        let collision = Catalog {
            servers: vec![server],
            ..baseline.clone()
        };
        assert!(load(&path, &collision, false).is_err());
        persist(&path, &BTreeMap::new()).await.unwrap();
        assert!(load(&path, &baseline, false).unwrap().is_empty());
        fs::write(&path, b"broken").unwrap();
        assert!(load(&path, &baseline, false).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
