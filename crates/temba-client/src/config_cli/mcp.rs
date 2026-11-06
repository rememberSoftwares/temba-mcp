use super::{Editor, assign, prompt};
use anyhow::{Context, Result, ensure};
use serde_yaml_ng::Value;
use std::{
    collections::HashSet,
    io::{BufRead, Write},
    path::Path,
};
use zeroize::Zeroizing;

struct Form<'a, R, W> {
    input: &'a mut R,
    output: &'a mut W,
    terminal: bool,
}
impl<R: BufRead, W: Write> Form<'_, R, W> {
    fn ask(&mut self, question: &str) -> Result<String> {
        prompt(self.input, self.output, question)
    }
    fn yes(&mut self, question: &str) -> Result<bool> {
        loop {
            match self
                .ask(&format!("{question} [y/N]"))?
                .to_lowercase()
                .as_str()
            {
                "y" | "yes" => return Ok(true),
                "" | "n" | "no" => return Ok(false),
                _ => writeln!(self.output, "Enter y or n")?,
            }
        }
    }
    fn secret(&mut self) -> Result<String> {
        write!(
            self.output,
            "Value{}: ",
            if self.terminal { " (hidden)" } else { "" }
        )?;
        self.output.flush()?;
        let value = if self.terminal {
            rpassword::read_password().context("Cannot read private value from the terminal")?
        } else {
            let mut line = String::new();
            ensure!(
                self.input.read_line(&mut line)? != 0,
                "Setup cancelled: input closed. No configuration changes were saved"
            );
            line.trim_end_matches(['\r', '\n']).to_owned()
        };
        if self.terminal {
            writeln!(self.output)?;
        }
        Ok(value)
    }
    fn nonempty(&mut self, question: &str) -> Result<String> {
        loop {
            let value = self.ask(question)?;
            if !value.is_empty() {
                return Ok(value);
            }
            writeln!(self.output, "A value is required")?;
        }
    }
    fn name(&mut self, existing: &HashSet<String>) -> Result<String> {
        loop {
            let name = self.ask("MCP name (unique within this catalog)")?;
            if !crate::config::valid_name(&name) {
                writeln!(
                    self.output,
                    "Use 1–64 letters, digits, hyphens or underscores, without a leading __"
                )?;
            } else if existing.contains(&name) {
                writeln!(
                    self.output,
                    "MCP name '{name}' already has an entry here. Choose another name; edit the YAML to update an existing entry"
                )?;
            } else {
                return Ok(name);
            }
        }
    }
    fn endpoint(&mut self, question: &str, optional: bool) -> Result<Option<String>> {
        loop {
            let value = self.ask(question)?;
            if value.is_empty() && optional {
                return Ok(None);
            }
            if temba_core::endpoint(&value, true).is_ok() {
                return Ok(Some(value));
            }
            writeln!(
                self.output,
                "Enter an HTTP or HTTPS endpoint without embedded credentials or a fragment"
            )?;
        }
    }
    fn timeout(&mut self) -> Result<u64> {
        loop {
            let value = self.ask("Connection and request timeout in seconds [60]")?;
            if value.is_empty() {
                return Ok(60);
            }
            if let Ok(value) = value.parse::<u64>()
                && (1..=300).contains(&value)
            {
                return Ok(value);
            }
            writeln!(self.output, "Enter a whole number between 1 and 300")?;
        }
    }
}
fn object() -> Value {
    Value::Mapping(Default::default())
}
fn put(target: &mut Value, key: &str, value: impl Into<Value>) -> Result<()> {
    assign(target, None, key, value.into())
}
fn sequence(values: Vec<String>) -> Value {
    Value::Sequence(values.into_iter().map(Value::String).collect())
}
fn env_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, b)| {
            b == b'_' || b.is_ascii_alphabetic() || (index > 0 && b.is_ascii_digit())
        })
}
fn environment_reference(name: String, variable: String) -> Value {
    serde_yaml_ng::to_value(
        serde_json::json!({"name":name,"valueFrom":{"envVarRef":{"name":variable}}}),
    )
    .unwrap()
}
fn variable<R: BufRead, W: Write>(form: &mut Form<'_, R, W>) -> Result<String> {
    loop {
        let name = form.ask("Environment variable to read on this computer")?;
        if env_name(&name) {
            return Ok(name);
        }
        writeln!(
            form.output,
            "Use an environment variable name such as SERVICE_TOKEN"
        )?;
    }
}
fn source<R: BufRead, W: Write>(form: &mut Form<'_, R, W>, public: bool) -> Result<u8> {
    writeln!(
        form.output,
        "  1) Environment variable\n  2) Private literal value"
    )?;
    if public {
        writeln!(form.output, "  3) Public literal value (not a secret)")?;
    }
    loop {
        match form.ask("Value source [1]")?.as_str() {
            "" | "1" => return Ok(1),
            "2" => return Ok(2),
            "3" if public => return Ok(3),
            _ => writeln!(form.output, "Choose one of the listed numbers")?,
        }
    }
}
fn headers<R: BufRead, W: Write>(
    form: &mut Form<'_, R, W>,
    local: bool,
    oauth: bool,
    endpoint: &str,
) -> Result<(Vec<Value>, Option<Value>)> {
    let mut declarations = Vec::new();
    let mut literal = object();
    let mut environment = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let name = form.ask("Header name (empty to finish)")?;
        if name.is_empty() {
            break;
        }
        let normalized = name.to_ascii_lowercase();
        let valid = reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
            && name.len() <= 128
            && (!local
                || ((normalized == "authorization" || normalized.starts_with("x-"))
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')));
        if !valid {
            writeln!(
                form.output,
                "Use a valid HTTP header name{}",
                if local { " (Authorization or X-*)" } else { "" }
            )?;
            continue;
        }
        if oauth && normalized == "authorization" {
            writeln!(
                form.output,
                "OAuth manages Authorization. Additional OAuth headers must use X-* names"
            )?;
            continue;
        }
        if seen.contains(&normalized) {
            writeln!(
                form.output,
                "This header is already configured (names are case-insensitive)"
            )?;
            continue;
        }
        let kind = source(form, local && normalized != "authorization")?;
        let mut declaration = object();
        put(&mut declaration, "name", name.clone())?;
        if kind == 1 {
            environment.push(environment_reference(name.clone(), variable(form)?));
        } else if kind == 2 {
            let value = Zeroizing::new(form.secret()?);
            ensure!(
                !value.is_empty(),
                "Header value cannot be empty. No changes were saved"
            );
            reqwest::header::HeaderValue::from_str(&value).map_err(|_|anyhow::anyhow!("Invalid header value; line breaks and control characters are not allowed. No changes were saved"))?;
            put(&mut literal, &name, value.as_str())?;
        } else {
            let value = form.ask("Public header value (not a secret)")?;
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
                anyhow::anyhow!("Invalid public header value. No changes were saved")
            })?;
            put(&mut declaration, "value", value)?;
        }
        if local {
            if kind != 3 {
                put(&mut declaration, "credential", name.clone())?;
                put(
                    &mut declaration,
                    "mandatory",
                    form.yes("Is this header mandatory")?,
                )?;
            }
            declarations.push(declaration);
        }
        seen.insert(normalized);
        ensure!(
            seen.len() <= 32,
            "At most 32 headers can be configured. No changes were saved"
        );
    }
    if literal.as_mapping().unwrap().is_empty() && environment.is_empty() {
        return Ok((declarations, None));
    }
    let mut private = object();
    put(&mut private, "expected_endpoint", endpoint)?;
    if !literal.as_mapping().unwrap().is_empty() {
        put(&mut private, "headers", literal)?;
    }
    if !environment.is_empty() {
        put(&mut private, "headersFromEnv", Value::Sequence(environment))?;
    }
    Ok((declarations, Some(private)))
}
fn stdio<R: BufRead, W: Write>(form: &mut Form<'_, R, W>, definition: &mut Value) -> Result<()> {
    put(
        definition,
        "command",
        form.nonempty("Executable (path or name on PATH; not a shell command line)")?,
    )?;
    writeln!(
        form.output,
        "Enter arguments separately. Quotes are literal; an empty answer finishes the list"
    )?;
    let mut args = Vec::new();
    loop {
        let arg = form.ask("Argument (empty to finish)")?;
        if arg.is_empty() {
            break;
        }
        args.push(arg);
        ensure!(args.len() <= 256, "At most 256 arguments are supported");
    }
    if !args.is_empty() {
        put(definition, "args", sequence(args))?;
    }
    loop {
        let cwd = form.ask("Working directory (absolute path, optional)")?;
        if cwd.is_empty() {
            break;
        }
        if Path::new(&cwd).is_absolute() {
            put(definition, "cwd", cwd)?;
            break;
        }
        writeln!(form.output, "Use an absolute working directory")?;
    }
    let mut literal = object();
    let mut environment = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let name = form.ask("Environment variable to pass to the MCP (empty to finish)")?;
        if name.is_empty() {
            break;
        }
        if !env_name(&name) || seen.contains(&name.to_ascii_uppercase()) {
            writeln!(form.output, "Use a unique environment variable name")?;
            continue;
        }
        match source(form, false)? {
            1 => environment.push(environment_reference(name.clone(), variable(form)?)),
            _ => {
                let value = Zeroizing::new(form.secret()?);
                put(&mut literal, &name, value.as_str())?;
            }
        }
        seen.insert(name.to_ascii_uppercase());
        ensure!(
            seen.len() <= 64,
            "At most 64 environment variables are supported"
        );
    }
    if !literal.as_mapping().unwrap().is_empty() {
        put(definition, "env", literal)?;
    }
    if !environment.is_empty() {
        put(definition, "envFromEnv", Value::Sequence(environment))?;
    }
    Ok(())
}
fn allow_http<R: BufRead, W: Write>(
    form: &mut Form<'_, R, W>,
    document: &mut Value,
    url: &str,
) -> Result<()> {
    if url.starts_with("http:") && document["allow_insecure_http"].as_bool() != Some(true) {
        ensure!(
            form.yes(
                "Enable allow_insecure_http for this configuration to use this HTTP endpoint"
            )?,
            "MCP not added. HTTP was not authorized; no changes were saved"
        );
        put(document, "allow_insecure_http", true)?;
    }
    Ok(())
}
fn attach_private(target: &mut Value, name: &str, private: Value) -> Result<()> {
    let mapping = target
        .as_mapping_mut()
        .context("Catalog must be a YAML mapping")?;
    let tools = mapping
        .entry(Value::String("tools".into()))
        .or_insert_with(object);
    put(tools, name, private)
}

pub fn add_mcp(
    path: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    terminal: bool,
) -> Result<()> {
    let mut editor = Editor::open(path)?;
    let remote_names: Vec<String> = editor.document["catalogs"]
        .as_sequence()
        .into_iter()
        .flatten()
        .filter_map(|v| v["name"].as_str().map(str::to_owned))
        .collect();
    let mut form = Form {
        input,
        output,
        terminal,
    };
    writeln!(
        form.output,
        "Choose a catalog:\n  1) local — HTTP or stdio MCP definitions on this computer"
    )?;
    for (index, name) in remote_names.iter().enumerate() {
        writeln!(
            form.output,
            "  {}) {} — local headers for an existing remote MCP",
            index + 2,
            name
        )?;
    }
    let selected = loop {
        let value = form.ask("Catalog number or name [local]")?;
        if value.is_empty() || value == "local" || value == "1" {
            break 0;
        }
        if let Ok(index) = value.parse::<usize>()
            && (2..=remote_names.len() + 1).contains(&index)
        {
            break index - 1;
        }
        if let Some(index) = remote_names.iter().position(|name| name == &value) {
            break index + 1;
        }
        writeln!(form.output, "Choose a catalog from the list")?;
    };
    if selected > 0 {
        writeln!(
            form.output,
            "\nThis configures local headers only. Use the exact MCP name and endpoint from 'temba catalog'. The shared catalog is unchanged; OAuth settings remain catalog-managed"
        )?;
        let target = &editor.document["catalogs"][selected - 1];
        let existing = target["tools"]
            .as_mapping()
            .into_iter()
            .flatten()
            .filter_map(|(k, _)| k.as_str().map(str::to_owned))
            .collect();
        let name = form.name(&existing)?;
        let endpoint = form
            .endpoint("MCP endpoint (as published by the catalog)", false)?
            .unwrap();
        allow_http(&mut form, &mut editor.document, &endpoint)?;
        let (_, private) = headers(&mut form, false, false, &endpoint)?;
        let private =
            private.context("No headers were provided. No configuration changes were saved")?;
        attach_private(
            &mut editor.document["catalogs"][selected - 1],
            &name,
            private,
        )?;
    } else {
        let mut existing: HashSet<String> = editor.document["local_catalog"]["servers"]
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|s| s["name"].as_str().map(str::to_owned))
            .collect();
        existing.extend(
            editor.document["local_catalog"]["tools"]
                .as_mapping()
                .into_iter()
                .flatten()
                .filter_map(|(name, _)| name.as_str().map(str::to_owned)),
        );
        let name = form.name(&existing)?;
        let description = form.ask("Description (optional)")?;
        let mut definition = object();
        put(&mut definition, "name", name.clone())?;
        if !description.is_empty() {
            put(&mut definition, "description", description)?;
        }
        let endpoint = form.endpoint(
            "MCP endpoint (leave empty to run a local stdio program)",
            true,
        )?;
        let private = if let Some(endpoint) = endpoint {
            allow_http(&mut form, &mut editor.document, &endpoint)?;
            put(&mut definition, "endpoint", endpoint.clone())?;
            let oauth = form.yes("Does this MCP use OAuth")?;
            if oauth {
                let mut settings = object();
                if form
                    .yes("Configure advanced OAuth settings (client ID, scopes, trusted origins)")?
                {
                    let client =
                        form.ask("OAuth client ID (optional; automatic registration if empty)")?;
                    if !client.is_empty() {
                        put(&mut settings, "clientId", client)?;
                    }
                    let scopes = form.ask("OAuth scopes (space-separated, optional)")?;
                    if !scopes.is_empty() {
                        put(
                            &mut settings,
                            "scopes",
                            sequence(scopes.split_whitespace().map(str::to_owned).collect()),
                        )?;
                    }
                    let origins=form.ask("Trusted OAuth origins (comma-separated, optional; empty for automatic public discovery)")?;
                    if !origins.is_empty() {
                        let origins: Vec<String> = origins
                            .split(',')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned)
                            .collect();
                        for origin in &origins {
                            let parsed = temba_core::endpoint(origin, true)
                                .context("Invalid trusted OAuth origin")?;
                            ensure!(
                                parsed.path() == "/" && parsed.query().is_none(),
                                "Trusted OAuth origins must contain only a scheme, host and optional port, without a path or query. No changes were saved"
                            );
                            allow_http(&mut form, &mut editor.document, origin)?;
                        }
                        put(&mut settings, "allowedOrigins", sequence(origins))?;
                    }
                }
                put(&mut definition, "oauth", settings)?;
            }
            let (declarations, private) = headers(&mut form, true, oauth, &endpoint)?;
            if !declarations.is_empty() {
                put(&mut definition, "headers", Value::Sequence(declarations))?;
            }
            private
        } else {
            stdio(&mut form, &mut definition)?;
            None
        };
        put(&mut definition, "timeoutSeconds", form.timeout()?)?;
        assign(
            &mut editor.document,
            Some("local_catalog"),
            "enabled",
            Value::Bool(true),
        )?;
        let local = &mut editor.document["local_catalog"];
        let mapping = local.as_mapping_mut().unwrap();
        let servers = mapping
            .entry(Value::String("servers".into()))
            .or_insert_with(|| Value::Sequence(Vec::new()));
        servers
            .as_sequence_mut()
            .context("local_catalog.servers must be a list")?
            .push(definition);
        if let Some(private) = private {
            attach_private(local, &name, private)?;
        }
    }
    writeln!(form.output)?;
    editor.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("temba-add-mcp-{}", uuid::Uuid::new_v4().simple())),
            )
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("temba.yaml")
        }
        fn load(&self) -> Value {
            serde_yaml_ng::from_str(&std::fs::read_to_string(self.path()).unwrap()).unwrap()
        }
        fn run(&self, answers: &str) -> Result<String> {
            let mut output = Vec::new();
            add_mcp(&self.path(), &mut answers.as_bytes(), &mut output, false)?;
            Ok(String::from_utf8(output).unwrap())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn local_oauth_and_headers_coexist_and_authorization_is_reserved() {
        let temp = Temp::new();
        let output=temp.run("\nservice\nService tools\nhttps://mcp.example/mcp\ny\nn\nAuthorization\nX-Token\n1\nSERVICE_TOKEN\ny\nX-Region\n3\neu-west\n\n\n").unwrap();
        let document = temp.load();
        let entry = &document["local_catalog"]["servers"][0];
        assert_eq!(document["local_catalog"]["enabled"], true);
        assert!(entry["oauth"].as_mapping().unwrap().is_empty());
        assert_eq!(entry["headers"][0]["name"], "X-Token");
        assert_eq!(entry["headers"][0]["mandatory"], true);
        assert_eq!(entry["headers"][1]["value"], "eu-west");
        assert_eq!(
            document["local_catalog"]["tools"]["service"]["headersFromEnv"][0]["valueFrom"]["envVarRef"]
                ["name"],
            "SERVICE_TOKEN"
        );
        assert!(output.contains("OAuth manages Authorization"));
    }
    #[test]
    fn optional_advanced_oauth_settings_are_saved_without_connecting() {
        let temp = Temp::new();
        temp.run("local\nsso\n\nhttps://mcp.example/mcp\ny\ny\npublic-client\nopenid profile\nhttps://auth.example\n\n\n").unwrap();
        let document = temp.load();
        let oauth = &document["local_catalog"]["servers"][0]["oauth"];
        assert_eq!(oauth["clientId"], "public-client");
        assert_eq!(oauth["scopes"][1], "profile");
        assert_eq!(oauth["allowedOrigins"][0], "https://auth.example");
        assert!(document["local_catalog"].get("tools").is_none());
    }
    #[test]
    fn private_header_values_are_preserved_without_being_printed() {
        let temp = Temp::new();
        let output=temp.run("1\nservice\n\nhttps://mcp.example/mcp\nn\nAuthorization\n2\nBearer PRIVATE_TEST_VALUE\ny\nauthorization\n\n\n").unwrap();
        assert!(!output.contains("PRIVATE_TEST_VALUE"));
        assert!(output.contains("case-insensitive"));
        assert_eq!(
            temp.load()["local_catalog"]["tools"]["service"]["headers"]["Authorization"],
            "Bearer PRIVATE_TEST_VALUE"
        );
        assert_eq!(
            temp.load()["local_catalog"]["servers"][0]["headers"][0]["mandatory"],
            true
        );
    }
    #[test]
    fn stdio_keeps_argument_boundaries_and_supports_env_references() {
        let temp = Temp::new();
        let cwd = std::env::temp_dir();
        let output=temp.run(&format!("local\nworker\n\n\npython3\nfixture.py\nhello world\n\nrelative/path\n{}\nTOKEN\n1\nSOURCE_TOKEN\nMODE\n2\ndevelopment\n\n120\n",cwd.display())).unwrap();
        let doc = temp.load();
        let entry = &doc["local_catalog"]["servers"][0];
        assert_eq!(entry["command"], "python3");
        assert_eq!(entry["args"][1], "hello world");
        assert_eq!(entry["cwd"], cwd.to_string_lossy().to_string());
        assert_eq!(entry["timeoutSeconds"], 120);
        assert_eq!(entry["env"]["MODE"], "development");
        assert_eq!(
            entry["envFromEnv"][0]["valueFrom"]["envVarRef"]["name"],
            "SOURCE_TOKEN"
        );
        assert!(output.contains("absolute working directory"));
        assert!(entry.get("headers").is_none());
    }
    #[test]
    fn remote_selection_only_adds_local_credentials_and_never_modifies_definitions() {
        let temp = Temp::new();
        std::fs::create_dir_all(&temp.0).unwrap();
        let initial = "catalogs:\n- name: team\n  url: https://catalog.example\n- name: '2'\n  url: https://second.example\nlocal_catalog:\n  enabled: true\n  servers:\n  - name: keep\n    endpoint: https://kept.example/mcp\n";
        std::fs::write(temp.path(), initial).unwrap();
        let before = temp.load();
        let output=temp.run("2\nservice\nhttps://remote.example/mcp\nAuthorization\n2\nBearer PRIVATE_TEST_VALUE\n\n").unwrap();
        let after = temp.load();
        assert_eq!(after["local_catalog"], before["local_catalog"]);
        assert_eq!(after["catalogs"][1], before["catalogs"][1]);
        assert_eq!(after["catalogs"][0]["url"], before["catalogs"][0]["url"]);
        assert!(after["catalogs"][0].get("servers").is_none());
        assert_eq!(
            after["catalogs"][0]["tools"]["service"]["expected_endpoint"],
            "https://remote.example/mcp"
        );
        assert!(!output.contains("PRIVATE_TEST_VALUE"));
        assert!(output.contains("shared catalog is unchanged"));
    }
    #[test]
    fn duplicates_cannot_replace_definitions_and_cancelled_setup_is_not_saved() {
        let temp = Temp::new();
        assert!(temp.run("1\nfresh\n").is_err());
        assert!(!temp.path().exists());
        temp.run("1\nfirst\n\nhttps://first.example/mcp\nn\n\n\n")
            .unwrap();
        let before = std::fs::read(temp.path()).unwrap();
        let output = temp
            .run("1\nfirst\nsecond\n\nhttps://second.example/mcp\nn\n\n0\n300\n")
            .unwrap();
        assert!(output.contains("already has an entry"));
        assert!(output.contains("between 1 and 300"));
        assert_eq!(
            temp.load()["local_catalog"]["servers"][0]["endpoint"],
            "https://first.example/mcp"
        );
        assert_eq!(
            temp.load()["local_catalog"]["servers"]
                .as_sequence()
                .unwrap()
                .len(),
            2
        );
        let after = std::fs::read(temp.path()).unwrap();
        assert_ne!(before, after);
        assert!(
            temp.run("1\nthird\n\nhttp://127.0.0.1:8000/mcp\nn\n")
                .is_err()
        );
        assert_eq!(std::fs::read(temp.path()).unwrap(), after);
        assert!(
            temp.run("1\nthird\n\nhttps://third.example/mcp\nn\nX-Token\n2\nBAD\0VALUE\n")
                .is_err()
        );
        assert_eq!(std::fs::read(temp.path()).unwrap(), after);
    }
}
