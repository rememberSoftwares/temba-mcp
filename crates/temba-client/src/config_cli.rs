//! Configuration setup, independent of the MCP runtime and network connections.
use anyhow::{Context, Result, bail, ensure};
use serde_yaml_ng::Value;
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const INITIAL: &str = "refresh_minutes: 60\nsearch_limit: 5\nallow_insecure_http: false\n\ncatalogs: []\n\ndynamic_catalog:\n  enabled: false\n  max_servers: 16\n\nlocal_catalog:\n  enabled: false\n  servers: []\n";

pub fn path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure!(
            !path.as_os_str().is_empty(),
            "Configuration path is empty. Supply --config FILE or unset an empty TEMBA_CONFIG"
        );
        return Ok(path.to_owned());
    }
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    default_path(home.as_deref().map(Path::new))
}
fn default_path(home: Option<&Path>) -> Result<PathBuf> {
    let home = home.filter(|p| !p.as_os_str().is_empty() && p.is_absolute())
        .context("Cannot locate your home directory. Set HOME (USERPROFILE on Windows), use --config /path/to/temba.yaml, or set TEMBA_CONFIG")?;
    Ok(home.join(".temba").join("temba.yaml"))
}
pub fn read(path: &Path) -> Result<Zeroizing<String>> {
    fs::read_to_string(path).map(Zeroizing::new).with_context(|| format!(
        "Cannot read Temba configuration '{}'. Run 'temba add catalog' to create the default configuration, or select your own file with --config /path/to/temba.yaml (or TEMBA_CONFIG)", path.display()))
}

struct Editor {
    path: PathBuf,
    document: Value,
    _lock: fs::File,
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn private_file() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
impl Editor {
    fn open(path: &Path) -> Result<Self> {
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory
            .create(parent(path))
            .context("Cannot create configuration directory")?;
        let lock_path = parent(path).join(format!(
            ".{}.lock",
            path.file_name()
                .context("Configuration path needs a filename")?
                .to_string_lossy()
        ));
        let lock = private_file()
            .create(true)
            .truncate(false)
            .open(lock_path)
            .context("Cannot open configuration lock")?;
        lock.try_lock().map_err(|_| anyhow::anyhow!("Cannot lock configuration '{}'; another configuration command may be editing it. Retry after it finishes", path.display()))?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "Configuration is a symbolic link. Use --config with its actual target to edit it"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect configuration file"),
        }
        let source = match fs::read_to_string(path) {
            Ok(source) => Zeroizing::new(source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Zeroizing::new(INITIAL.to_owned())
            }
            Err(error) => return Err(error).context("Cannot read configuration for editing"),
        };
        crate::diagnostics::config(&source, &path.display().to_string())?;
        let document =
            serde_yaml_ng::from_str(&source).context("Cannot load validated configuration")?;
        Ok(Self {
            path: path.to_owned(),
            document,
            _lock: lock,
        })
    }
    fn save(&self) -> Result<()> {
        let yaml = Zeroizing::new(
            serde_yaml_ng::to_string(&self.document).context("Cannot serialize configuration")?,
        );
        let mut serialized = Zeroizing::new(String::with_capacity(yaml.len() + 3));
        for line in yaml.split_inclusive('\n') {
            if ["catalogs:", "dynamic_catalog:", "local_catalog:"]
                .iter()
                .any(|section| line.starts_with(section))
                && !serialized.is_empty()
                && !serialized.ends_with("\n\n")
            {
                serialized.push('\n');
            }
            serialized.push_str(line);
        }
        crate::diagnostics::config(&serialized, &self.path.display().to_string())?;
        let temporary =
            parent(&self.path).join(format!(".temba-{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            let mut file = private_file().create_new(true).open(&temporary)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| {
            format!(
                "Cannot save configuration '{}'; the previous file was kept",
                self.path.display()
            )
        })
    }
}

fn prompt(input: &mut impl BufRead, output: &mut impl Write, question: &str) -> Result<String> {
    write!(output, "{question}: ")?;
    output.flush()?;
    let mut line = String::new();
    ensure!(
        input.read_line(&mut line)? != 0,
        "Setup cancelled: input closed. No configuration changes were saved"
    );
    Ok(line.trim().to_owned())
}
fn assign(document: &mut Value, section: Option<&str>, key: &str, value: Value) -> Result<()> {
    let target = if let Some(section) = section {
        let root = document
            .as_mapping_mut()
            .context("Configuration must be a YAML mapping")?;
        root.entry(Value::String(section.into()))
            .or_insert_with(|| Value::Mapping(Default::default()))
    } else {
        document
    };
    if target.is_null() {
        *target = Value::Mapping(Default::default());
    }
    target
        .as_mapping_mut()
        .context("Configuration section must be a YAML mapping")?
        .insert(Value::String(key.into()), value);
    Ok(())
}
pub fn set(path: &Path, key: &str, value: &str) -> Result<()> {
    let (section, field, value) = match key {
        "dynamic_catalog"
        | "dynamic_catalog.enabled"
        | "local_catalog.enabled"
        | "allow_insecure_http" => {
            let boolean = value
                .parse::<bool>()
                .context("Expected true or false (lowercase)")?;
            let (section, field) = match key {
                "dynamic_catalog" | "dynamic_catalog.enabled" => {
                    (Some("dynamic_catalog"), "enabled")
                }
                "local_catalog.enabled" => (Some("local_catalog"), "enabled"),
                _ => (None, "allow_insecure_http"),
            };
            (section, field, Value::Bool(boolean))
        }
        "search_limit" | "refresh_minutes" | "dynamic_catalog.max_servers" => {
            let integer = value
                .parse::<u64>()
                .context("Expected a nonnegative whole number")?;
            let (section, field) = if key == "dynamic_catalog.max_servers" {
                (Some("dynamic_catalog"), "max_servers")
            } else {
                (None, key)
            };
            (section, field, Value::Number(integer.into()))
        }
        _ => bail!(
            "Unknown setting. Use search_limit, refresh_minutes, allow_insecure_http, dynamic_catalog, dynamic_catalog.enabled, dynamic_catalog.max_servers or local_catalog.enabled; see 'temba set --help'"
        ),
    };
    let mut editor = Editor::open(path)?;
    assign(&mut editor.document, section, field, value)?;
    editor.save()
}
pub fn add_catalog(path: &Path, input: &mut impl BufRead, output: &mut impl Write) -> Result<()> {
    let mut editor = Editor::open(path)?;
    let url = prompt(input, output, "Catalog URL (leave empty for local catalog)")?;
    if url.is_empty() {
        assign(
            &mut editor.document,
            Some("local_catalog"),
            "enabled",
            Value::Bool(true),
        )?;
        let local = editor.document["local_catalog"].as_mapping_mut().unwrap();
        local
            .entry(Value::String("servers".into()))
            .or_insert_with(|| Value::Sequence(Vec::new()));
        editor.save()?;
        writeln!(
            output,
            "\nLocal catalog enabled. Add your MCP definitions under local_catalog.servers in '{}'.",
            path.display()
        )?;
        return Ok(());
    }
    let parsed = temba_core::endpoint(&url, true).context(
        "Invalid catalog URL: use an HTTP or HTTPS URL without credentials or a fragment",
    )?;
    ensure!(
        parsed.query().is_none(),
        "Catalog URL cannot contain a query; use the base catalog URL"
    );
    if parsed.scheme() == "http" && editor.document["allow_insecure_http"].as_bool() != Some(true) {
        let answer = prompt(
            input,
            output,
            "This URL uses unencrypted HTTP. Enable allow_insecure_http for this configuration? [y/N]",
        )?;
        ensure!(
            matches!(answer.to_lowercase().as_str(), "y" | "yes"),
            "Catalog not added. Use an HTTPS URL or explicitly allow HTTP. No changes were saved"
        );
        assign(
            &mut editor.document,
            None,
            "allow_insecure_http",
            Value::Bool(true),
        )?;
    }
    let name = loop {
        let name = prompt(input, output, "Catalog name (unique, e.g. team)")?;
        if !crate::config::valid_name(&name) || name == "local" {
            writeln!(
                output,
                "Use 1–64 letters, digits, hyphens or underscores, without a leading __. The name 'local' is reserved"
            )?;
        } else if editor.document["catalogs"]
            .as_sequence()
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry["name"].as_str() == Some(&name))
            })
        {
            writeln!(
                output,
                "Catalog name '{name}' already exists. Choose a different name"
            )?;
        } else {
            break name;
        }
    };
    let labels = prompt(input, output, "Labels (comma-separated, optional)")?;
    writeln!(output)?;
    let labels: Vec<Value> = labels
        .split(',')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Value::String(l.to_lowercase()))
        .collect();
    let mut entry = serde_yaml_ng::Mapping::new();
    entry.insert(Value::String("name".into()), Value::String(name));
    entry.insert(
        Value::String("url".into()),
        Value::String(url.trim_end_matches('/').into()),
    );
    entry.insert(Value::String("labels".into()), Value::Sequence(labels));
    let root = editor
        .document
        .as_mapping_mut()
        .context("Configuration must be a YAML mapping")?;
    let catalogs = root
        .entry(Value::String("catalogs".into()))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    catalogs
        .as_sequence_mut()
        .context("catalogs must be a YAML list")?
        .push(Value::Mapping(entry));
    editor.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("temba-cli-{}", uuid::Uuid::new_v4().simple())))
        }
        fn config(&self) -> PathBuf {
            self.0.join("nested").join("temba.yaml")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn load(path: &Path) -> Value {
        serde_yaml_ng::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }
    #[test]
    fn home_and_explicit_paths_do_not_depend_on_working_directory() {
        let home = std::env::temp_dir().join("User name é");
        assert_eq!(
            default_path(Some(&home)).unwrap(),
            home.join(".temba").join("temba.yaml")
        );
        assert!(default_path(None).is_err());
        assert!(default_path(Some(Path::new(""))).is_err());
        assert!(default_path(Some(Path::new("relative"))).is_err());
        assert_eq!(
            path(Some(Path::new("custom.yaml"))).unwrap(),
            PathBuf::from("custom.yaml")
        );
    }
    #[test]
    fn wizard_adds_catalogs_retries_duplicate_names_and_enables_local() {
        let temporary = Temp::new();
        let path = temporary.config();
        let mut output = Vec::new();
        add_catalog(
            &path,
            &mut &b"https://catalog.example/\nteam\nTech, support\n"[..],
            &mut output,
        )
        .unwrap();
        let first = load(&path);
        assert_eq!(first["catalogs"][0]["name"], "team");
        assert_eq!(first["catalogs"][0]["url"], "https://catalog.example");
        assert_eq!(first["catalogs"][0]["labels"][0], "tech");
        add_catalog(
            &path,
            &mut &b"https://other.example\nteam\nlocal\nother\n\n"[..],
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("already exists"));
        assert!(text.contains("reserved"));
        assert_eq!(load(&path)["catalogs"][1]["name"], "other");
        add_catalog(&path, &mut &b"\n"[..], &mut Vec::new()).unwrap();
        assert_eq!(load(&path)["local_catalog"]["enabled"], true);
        assert_eq!(load(&path)["catalogs"].as_sequence().unwrap().len(), 2);
        assert_eq!(load(&path)["catalogs"][0], first["catalogs"][0]);
    }
    #[test]
    fn setting_validates_ranges_and_preserves_unrelated_private_values() {
        let temporary = Temp::new();
        let path = temporary.config();
        set(&path, "dynamic_catalog", "true").unwrap();
        assert_eq!(load(&path)["dynamic_catalog"]["enabled"], true);
        let mut document = load(&path);
        document["catalogs"] = serde_yaml_ng::from_str("- name: team\n  url: https://catalog.example\n  tools:\n    service:\n      expected_endpoint: https://mcp.example\n      headers: {Authorization: PRIVATE_TEST_VALUE}").unwrap();
        fs::write(&path, serde_yaml_ng::to_string(&document).unwrap()).unwrap();
        set(&path, "search_limit", "10").unwrap();
        assert_eq!(load(&path)["catalogs"], document["catalogs"]);
        assert_eq!(load(&path)["search_limit"], 10);
        let before = fs::read(&path).unwrap();
        for (key, value) in [
            ("search_limit", "0"),
            ("refresh_minutes", "1441"),
            ("dynamic_catalog.max_servers", "65"),
            ("dynamic_catalog", "yes"),
            ("search_limit", "TOKEN_SECRET"),
            ("anything", "true"),
        ] {
            let error = set(&path, key, value).unwrap_err().to_string();
            assert!(!error.contains("TOKEN_SECRET"));
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        set(&path, "refresh_minutes", "1440").unwrap();
        set(&path, "dynamic_catalog.max_servers", "64").unwrap();
        assert_eq!(load(&path)["refresh_minutes"], 1440);
    }
    #[test]
    fn cancelled_setup_and_http_refusal_do_not_save_partial_changes() {
        let temporary = Temp::new();
        let path = temporary.config();
        assert!(
            add_catalog(
                &path,
                &mut &b"https://catalog.example\n"[..],
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(!path.exists());
        set(&path, "search_limit", "5").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(
            add_catalog(
                &path,
                &mut &b"http://localhost:8080\n\n"[..],
                &mut Vec::new()
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        add_catalog(
            &path,
            &mut &b"http://localhost:8080\ny\ndev\n\n"[..],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(load(&path)["allow_insecure_http"], true);
    }
    #[test]
    fn concurrent_edits_and_invalid_existing_files_are_rejected() {
        let temporary = Temp::new();
        let path = temporary.config();
        let lock = Editor::open(&path).unwrap();
        assert!(set(&path, "search_limit", "10").is_err());
        assert!(!path.exists());
        drop(lock);
        fs::write(&path, "search_limit: PRIVATE_TEST_VALUE").unwrap();
        let error = format!("{:#}", set(&path, "search_limit", "10").unwrap_err());
        assert!(!error.contains("PRIVATE_TEST_VALUE"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "search_limit: PRIVATE_TEST_VALUE"
        );
    }
    #[cfg(unix)]
    #[test]
    fn saved_credentials_are_private_and_symlinks_are_not_replaced() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = Temp::new();
        let path = temporary.config();
        set(&path, "search_limit", "10").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let link = path.with_file_name("linked.yaml");
        symlink(&path, &link).unwrap();
        assert!(set(&link, "search_limit", "11").is_err());
        assert_eq!(load(&path)["search_limit"], 10);
    }
}
