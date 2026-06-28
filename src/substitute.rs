use std::fmt;
use std::sync::Arc;

use minijinja::value::{Enumerator, ObjectRepr};
use minijinja::{value::Object, Environment, UndefinedBehavior, Value};

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
    let ctx = TrackingContext::new(provider);

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_keep_trailing_newline(true);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    #[test]
    fn returns_the_input_unchanged() {
        let provider = create_provider();
        let res = substitute("foo\nbar\n", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo\nbar\n".to_string());
    }

    #[test]
    fn substitutes_single_variable() {
        let provider = create_provider();
        let res = substitute("foo {{url}}\nbar\n", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo example.com\nbar\n".to_string());
    }

    #[test]
    fn substitutes_integer() {
        let provider = create_provider();
        let res = substitute("foo={{integer}}", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo=42".to_string());
    }

    #[test]
    fn substitutes_placeholder_with_default_value() {
        let provider = create_provider();
        let res = substitute(
            "foo: {{ url | fallback('fallback.com') }}\n",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "foo: example.com\n".to_string());
    }

    #[test]
    fn substitutes_default_value() {
        let provider = create_provider();
        let res = substitute(
            "foo: {{ href | fallback('fallback.com') }}\n",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "foo: [fallback: fallback.com]\n".to_string());
    }

    #[test]
    fn returns_value_missing_for_missing_variable() {
        let provider = create_provider();
        let res = substitute("foo: {{ href }}\n", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo: [missing: href]\n".to_string());
    }

    #[test]
    fn substitutes_single_variable_with_spaces() {
        let provider = create_provider();
        let res =
            substitute("foo {{ url  }}\nbar\n", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo example.com\nbar\n".to_string());
    }

    #[test]
    fn substitutes_one_variable_per_line() {
        let provider = create_provider();
        let res =
            substitute("foo {{url}}\nbar {{token}}\n", Arc::new(provider))
                .unwrap();

        assert_eq!(res, "foo example.com\nbar abc123\n".to_string());
    }

    #[test]
    fn substitutes_variable_on_the_same_line() {
        let provider = create_provider();
        let res =
            substitute("foo {{url}}, bar {{token}}\n", Arc::new(provider))
                .unwrap();

        assert_eq!(res, "foo example.com, bar abc123\n".to_string());
    }

    #[test]
    fn substitutes_variable_with_underscore_and_number_in_name() {
        let provider = create_provider();
        let res =
            substitute("foo: {{ api_url1 }}", Arc::new(provider)).unwrap();

        assert_eq!(res, "foo: foo.com".to_string());
    }

    #[test]
    fn substitutes_list_joined() {
        let provider = create_provider();
        let res = substitute(
            "foo: {{ list | select_multiple | join('') }}",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "foo: 123".to_string());
    }

    #[test]
    fn substitutes_comma_separated_list() {
        let provider = create_provider();
        let res = substitute(
            "foo: [ {{ list | select_multiple | join(', ') }} ]",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "foo: [ 1, 2, 3 ]".to_string());
    }

    #[test]
    fn substitutes_list_quoted_join() {
        let provider = create_provider();
        let res = substitute(
            r#"foo: {{ list | select_multiple }}"#,
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, r#"foo: ["1", "2", "3"]"#.to_string());
    }

    #[test]
    fn substitutes_list_of_objects() {
        let provider = create_provider();
        let res = substitute(
            r#"{% for l in label %}"{{ l.value }}"{% if not loop.last %}, {% endif %}{% endfor %}"#,
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, r#""bug", "docs""#.to_string());
    }

    #[test]
    fn returns_value_missing_when_var_missing_but_other_has_default() {
        let provider = create_provider();
        let res = substitute(
            "{{ with_default | fallback('x') }} {{ missing }}",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "[fallback: x] [missing: missing]".to_string());
    }

    #[test]
    fn fallback_filter_is_noop_when_value_present() {
        let provider = create_provider();
        let res = substitute(
            "{{ url | fallback('fallback.com') }}",
            Arc::new(provider),
        )
        .unwrap();

        assert_eq!(res, "example.com".to_string());
    }

    #[test]
    fn fails_for_template_syntax_error() {
        let provider = create_provider();
        let res = substitute("{% if %}", Arc::new(provider));

        assert!(res.is_err());
    }
}
