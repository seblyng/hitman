use std::{
    fmt,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

use minijinja::value::{Enumerator, ObjectRepr};
use minijinja::{
    path_loader, value::Object, Environment, Error, ErrorKind,
    UndefinedBehavior, Value,
};

#[derive(Debug, Clone)]
pub enum SubstituteValue {
    Single(Value),
    Multiple(Vec<toml::Value>),
}

pub trait SubstituteProvider {
    fn lookup_value(&self, key: &str) -> Option<SubstituteValue>;
    fn prompt(
        &self,
        key: &str,
        fallback: Option<&str>,
    ) -> anyhow::Result<Value>;
    fn select_single(
        &self,
        key: &str,
        values: &[toml::Value],
    ) -> anyhow::Result<Value>;
    fn select_multiple(
        &self,
        key: &str,
        values: &[toml::Value],
    ) -> anyhow::Result<Vec<Value>>;
}

impl fmt::Debug for dyn SubstituteProvider + Send + Sync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{SubstituteProvider}}")
    }
}

#[derive(Debug)]
struct TrackingContext {
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
}

impl TrackingContext {
    fn new(
        provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
    ) -> Self {
        Self { provider }
    }
}

impl Object for TrackingContext {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let key_str = key.as_str()?;

        // Returning None lets MiniJinja resolve registered global functions.
        if key_str == "shell" {
            return None;
        }

        let res = match self.provider.lookup_value(key_str) {
            None => Value::from_object(PendingValue {
                provider: self.provider.clone(),
                key: key_str.to_string(),
                fallback: None,
            }),
            Some(val) => match val {
                SubstituteValue::Single(value) => value,
                SubstituteValue::Multiple(values) => Value::from_object({
                    SingleSelect {
                        provider: self.provider.clone(),
                        key: key_str.to_string(),
                        values,
                    }
                }),
            },
        };
        Some(res)
    }
}

#[derive(Debug, Clone)]
pub struct PendingValue {
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
    key: String,
    fallback: Option<String>,
}

impl PendingValue {
    fn with_fallback(&self, value: String) -> Self {
        Self {
            provider: self.provider.clone(),
            key: self.key.clone(),
            fallback: Some(value),
        }
    }
}

impl Object for PendingValue {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(
        self: &Arc<Self>,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let value = self
            .provider
            .prompt(&self.key, self.fallback.as_deref())
            .unwrap();
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone)]
pub struct SingleSelect {
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
    key: String,
    values: Vec<toml::Value>,
}

impl Object for SingleSelect {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    // Allow Jinja built-ins like 'for' and 'map' on the entire list
    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let values = self.values.iter().map(Value::from_serialize).collect();
        Enumerator::Values(values)
    }

    fn render(
        self: &Arc<Self>,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let value = self
            .provider
            .select_single(&self.key, &self.values)
            .unwrap();
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone)]
pub struct MultiSelect {
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
    key: String,
    values: Vec<toml::Value>,
}

impl Object for MultiSelect {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        if let Ok(values) =
            self.provider.select_multiple(&self.key, &self.values)
        {
            Enumerator::Values(values)
        } else {
            Enumerator::NonEnumerable
        }
    }
}

pub fn substitute(
    input: &str,
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
) -> anyhow::Result<String> {
    substitute_in(input, provider, Path::new("."))
}

pub fn substitute_in(
    input: &str,
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
    working_dir: &Path,
) -> anyhow::Result<String> {
    let ctx = TrackingContext::new(provider);

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_keep_trailing_newline(true);
    env.set_loader(path_loader(working_dir));

    let function_working_dir = working_dir.to_owned();
    env.add_function("shell", move |command: String| {
        run_shell(&command, None, &function_working_dir)
    });

    let filter_working_dir = working_dir.to_owned();
    env.add_filter("shell", move |input: String, command: String| {
        run_shell(&command, Some(input), &filter_working_dir)
    });

    env.add_filter("select_multiple", |v: Value| {
        if let Some(obj) = v.downcast_object_ref::<SingleSelect>() {
            Value::from_object({
                MultiSelect {
                    provider: obj.provider.clone(),
                    key: obj.key.clone(),
                    values: obj.values.clone(),
                }
            })
        } else if v.downcast_object_ref::<MultiSelect>().is_some() {
            v
        } else {
            eprintln!("WARNING: Not multiple choice");
            v
        }
    });
    env.add_filter("select_one", |v: Value| v);

    env.add_filter("fallback", move |v: Value, fallback: String| {
        if let Some(obj) = v.downcast_object_ref::<PendingValue>() {
            Value::from_object(obj.with_fallback(fallback))
        } else {
            v
        }
    });

    let ctx_val = Value::from_object(ctx);

    Ok(env.render_str(input, ctx_val)?)
}

fn run_shell(
    command: &str,
    input: Option<String>,
    working_dir: &Path,
) -> Result<String, Error> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| shell_error(command, error))?;

    // Write concurrently so a command can produce output while consuming a
    // payload larger than the operating system's pipe buffer.
    let stdin_writer = input.map(|input| {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        thread::spawn(move || stdin.write_all(input.as_bytes()))
    });

    let output = child
        .wait_with_output()
        .map_err(|error| shell_error(command, error))?;

    if let Some(writer) = stdin_writer {
        match writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if output.status.success() => {
                return Err(shell_error(command, error));
            }
            Ok(Err(_)) => {}
            Err(_) => {
                return Err(shell_error(command, "stdin writer panicked"));
            }
        }
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let message = if detail.is_empty() {
            format!("shell command `{command}` exited with {}", output.status)
        } else {
            format!(
                "shell command `{command}` exited with {}: {detail}",
                output.status
            )
        };
        return Err(Error::new(ErrorKind::InvalidOperation, message));
    }

    String::from_utf8(output.stdout)
        .map_err(|error| shell_error(command, error))
}

fn shell_error(command: &str, error: impl fmt::Display) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("shell command `{command}` failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    use mktemp::Temp;

    struct TestProvider {
        vars: HashMap<String, SubstituteValue>,
    }

    impl SubstituteProvider for TestProvider {
        fn lookup_value(&self, key: &str) -> Option<SubstituteValue> {
            self.vars.get(key).cloned()
        }

        fn prompt(
            &self,
            key: &str,
            fallback: Option<&str>,
        ) -> anyhow::Result<Value> {
            if let Some(fb) = fallback {
                Ok(Value::from(format!("[fallback: {fb}]")))
            } else {
                Ok(Value::from(format!("[missing: {key}]")))
            }
        }

        fn select_single(
            &self,
            key: &str,
            values: &[toml::Value],
        ) -> anyhow::Result<Value> {
            if let Some(v) = values.first() {
                Ok(Value::from(v.as_str()))
            } else {
                anyhow::bail!("No value for {key}")
            }
        }

        fn select_multiple(
            &self,
            _key: &str,
            values: &[toml::Value],
        ) -> anyhow::Result<Vec<Value>> {
            Ok(values.iter().map(|v| Value::from(v.as_str())).collect())
        }
    }

    fn create_vars() -> HashMap<String, SubstituteValue> {
        let mut vars = HashMap::new();

        use SubstituteValue::{Multiple, Single};

        vars.insert("url".to_string(), Single(Value::from("example.com")));
        vars.insert("token".to_string(), Single(Value::from("abc123")));
        vars.insert("integer".to_string(), Single(Value::from(42i64)));
        vars.insert("api_url1".to_string(), Single(Value::from("foo.com")));
        vars.insert(
            "list".to_string(),
            Multiple(vec![
                toml::Value::from("1"),
                toml::Value::from("2"),
                toml::Value::from("3"),
            ]),
        );
        vars.insert(
            "label".to_string(),
            Single(Value::from_serialize(vec![
                serde_json::json!({"value": "bug", "name": "bug"}),
                serde_json::json!({"value": "docs", "name": "documentation"}),
            ])),
        );

        vars
    }

    fn create_provider() -> TestProvider {
        TestProvider {
            vars: create_vars(),
        }
    }

    fn render(input: &str, provider: TestProvider) -> anyhow::Result<String> {
        substitute(input, Arc::new(provider))
    }

    #[test]
    fn returns_the_input_unchanged() {
        let provider = create_provider();
        let res = render("foo\nbar\n", provider).unwrap();

        assert_eq!(res, "foo\nbar\n".to_string());
    }

    #[test]
    fn substitutes_single_variable() {
        let provider = create_provider();
        let res = render("foo {{url}}\nbar\n", provider).unwrap();

        assert_eq!(res, "foo example.com\nbar\n".to_string());
    }

    #[test]
    fn substitutes_integer() {
        let provider = create_provider();
        let res = render("foo={{integer}}", provider).unwrap();

        assert_eq!(res, "foo=42".to_string());
    }

    #[test]
    fn substitutes_placeholder_with_default_value() {
        let provider = create_provider();
        let res =
            render("foo: {{ url | fallback('fallback.com') }}\n", provider)
                .unwrap();

        assert_eq!(res, "foo: example.com\n".to_string());
    }

    #[test]
    fn substitutes_default_value() {
        let provider = create_provider();
        let res =
            render("foo: {{ href | fallback('fallback.com') }}\n", provider)
                .unwrap();

        assert_eq!(res, "foo: [fallback: fallback.com]\n".to_string());
    }

    #[test]
    fn returns_value_missing_for_missing_variable() {
        let provider = create_provider();
        let res = render("foo: {{ href }}\n", provider).unwrap();

        assert_eq!(res, "foo: [missing: href]\n".to_string());
    }

    #[test]
    fn substitutes_single_variable_with_spaces() {
        let provider = create_provider();
        let res = render("foo {{ url  }}\nbar\n", provider).unwrap();

        assert_eq!(res, "foo example.com\nbar\n".to_string());
    }

    #[test]
    fn substitutes_one_variable_per_line() {
        let provider = create_provider();
        let res = render("foo {{url}}\nbar {{token}}\n", provider).unwrap();

        assert_eq!(res, "foo example.com\nbar abc123\n".to_string());
    }

    #[test]
    fn substitutes_variable_on_the_same_line() {
        let provider = create_provider();
        let res = render("foo {{url}}, bar {{token}}\n", provider).unwrap();

        assert_eq!(res, "foo example.com, bar abc123\n".to_string());
    }

    #[test]
    fn substitutes_variable_with_underscore_and_number_in_name() {
        let provider = create_provider();
        let res = render("foo: {{ api_url1 }}", provider).unwrap();

        assert_eq!(res, "foo: foo.com".to_string());
    }

    #[test]
    fn substitutes_list_joined() {
        let provider = create_provider();
        let res =
            render("foo: {{ list | select_multiple | join('') }}", provider)
                .unwrap();

        assert_eq!(res, "foo: 123".to_string());
    }

    #[test]
    fn substitutes_comma_separated_list() {
        let provider = create_provider();
        let res = render(
            "foo: [ {{ list | select_multiple | join(', ') }} ]",
            provider,
        )
        .unwrap();

        assert_eq!(res, "foo: [ 1, 2, 3 ]".to_string());
    }

    #[test]
    fn substitutes_list_quoted_join() {
        let provider = create_provider();
        let res =
            render(r#"foo: {{ list | select_multiple }}"#, provider).unwrap();

        assert_eq!(res, r#"foo: ["1", "2", "3"]"#.to_string());
    }

    #[test]
    fn substitutes_list_of_objects() {
        let provider = create_provider();
        let res = render(
            r#"{% for l in label %}"{{ l.value }}"{% if not loop.last %}, {% endif %}{% endfor %}"#,
            provider,
        )
        .unwrap();

        assert_eq!(res, r#""bug", "docs""#.to_string());
    }

    #[test]
    fn returns_value_missing_when_var_missing_but_other_has_default() {
        let provider = create_provider();
        let res = render(
            "{{ with_default | fallback('x') }} {{ missing }}",
            provider,
        )
        .unwrap();

        assert_eq!(res, "[fallback: x] [missing: missing]".to_string());
    }

    #[test]
    fn fallback_filter_is_noop_when_value_present() {
        let provider = create_provider();
        let res =
            render("{{ url | fallback('fallback.com') }}", provider).unwrap();

        assert_eq!(res, "example.com".to_string());
    }

    #[test]
    fn fails_for_template_syntax_error() {
        let provider = create_provider();
        let res = render("{% if %}", provider);

        assert!(res.is_err());
    }

    #[test]
    fn runs_shell_as_a_function() {
        let res = render(
            r#"{{ shell("printf 'hello from shell'") }}"#,
            create_provider(),
        )
        .unwrap();

        assert_eq!(res, "hello from shell");
    }

    #[test]
    fn runs_shell_as_a_filter_with_the_value_as_stdin() {
        let res = render(
            r#"{{ "hello from filter" | shell("tr '[:lower:]' '[:upper:]'") }}"#,
            create_provider(),
        )
        .unwrap();

        assert_eq!(res, "HELLO FROM FILTER");
    }

    #[test]
    fn pipes_an_included_file_to_shell() {
        let tmp = Temp::new_dir().unwrap();
        fs::write(tmp.join("payload.json"), "{\n  \"name\": \"Hitman\"\n}\n")
            .unwrap();
        let input = r#"{% filter shell("tr '[:lower:]' '[:upper:]'") %}{% include "payload.json" %}{% endfilter %}"#;

        let res =
            substitute_in(input, Arc::new(create_provider()), &tmp).unwrap();

        assert_eq!(res, "{\n  \"NAME\": \"HITMAN\"\n}\n");
    }

    #[test]
    fn reports_shell_command_failures() {
        let error = render(
            r#"{{ shell("printf 'bad command' >&2; exit 7") }}"#,
            create_provider(),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("exit status: 7"));
        assert!(message.contains("bad command"));
    }
}
