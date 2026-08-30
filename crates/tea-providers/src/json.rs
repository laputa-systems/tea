//! Small Miniserde-backed helpers for private adapter JSON trees.

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
use tea_protocol::JsonError;
use tea_protocol::JsonNumber;
pub(crate) use tea_protocol::JsonValue;

/// Convert scalar expressions used by adapter payload builders into protocol JSON values.
pub(crate) trait JsonScalar {
    fn to_json_value(&self) -> JsonValue;
}

impl<T> JsonScalar for &T
where
    T: JsonScalar + ?Sized,
{
    fn to_json_value(&self) -> JsonValue {
        (*self).to_json_value()
    }
}

impl JsonScalar for JsonValue {
    fn to_json_value(&self) -> JsonValue {
        self.clone()
    }
}

impl JsonScalar for str {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::String(self.to_owned())
    }
}

impl JsonScalar for String {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::String(self.clone())
    }
}

impl JsonScalar for bool {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::Bool(*self)
    }
}

impl JsonScalar for u64 {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::from(*self)
    }
}

impl JsonScalar for usize {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::from(*self as u64)
    }
}

impl JsonScalar for i64 {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::from(*self)
    }
}

impl JsonScalar for i32 {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::from(*self as i64)
    }
}

impl JsonScalar for u32 {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::from(*self as u64)
    }
}

impl JsonScalar for f64 {
    fn to_json_value(&self) -> JsonValue {
        JsonValue::number(JsonNumber::Float(*self)).expect("adapter JSON numbers are finite")
    }
}

impl<T> JsonScalar for Option<T>
where
    T: JsonScalar,
{
    fn to_json_value(&self) -> JsonValue {
        self.as_ref()
            .map(JsonScalar::to_json_value)
            .unwrap_or(JsonValue::Null)
    }
}

impl<T> JsonScalar for Vec<T>
where
    T: JsonScalar,
{
    fn to_json_value(&self) -> JsonValue {
        JsonValue::Array(self.iter().map(JsonScalar::to_json_value).collect())
    }
}

/// Convert a scalar expression without moving a field out of an adapter config.
pub(crate) fn scalar<T>(value: &T) -> JsonValue
where
    T: JsonScalar + ?Sized,
{
    value.to_json_value()
}

/// Encode a protocol JSON tree for a byte-oriented transport.
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
pub(crate) fn to_bytes(value: &JsonValue) -> Result<Vec<u8>, JsonError> {
    value.to_json_string().map(String::into_bytes)
}

/// Parse one complete JSON document from transport bytes.
#[allow(dead_code)]
pub(crate) fn from_bytes(bytes: &[u8]) -> Result<JsonValue, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "response was not UTF-8".to_owned())?;
    JsonValue::parse(text).map_err(|_| "response was not valid JSON".to_owned())
}

macro_rules! json_value {
    (null) => {
        $crate::json::JsonValue::Null
    };
    ({ $($key:literal : $value:expr),* $(,)? }) => {
        $crate::json::JsonValue::object([
            $(($key, $crate::json::json_value!($value))),*
        ])
    };
    ([ $($value:expr),* $(,)? ]) => {
        $crate::json::JsonValue::Array(vec![
            $($crate::json::json_value!($value)),*
        ])
    };
    ($value:expr) => {
        $crate::json::scalar(&$value)
    };
}

pub(crate) use json_value;
