//! Luau values and JSON records, by the API's JSON conventions
//! (`docs/api.md`): records are tables with string keys, lists are
//! sequences, enums are their names, bytes are base64 strings.
//!
//! Luau numbers are doubles. A whole number becomes a JSON integer when it
//! is exactly representable (|n| <= 2^53); a larger one is refused rather
//! than silently rounded. NaN and infinities are refused.

use mlua::{Lua, Value};

/// Largest integer a double holds exactly.
const EXACT: f64 = 9_007_199_254_740_992.0;

/// A Luau value as JSON. `top` is a call's parameter record: an empty table
/// there is `{}`; elsewhere an empty table is `[]`.
pub fn to_json(v: &Value, top: bool) -> Result<serde_json::Value, String> {
    to_json_at(v, top, 0)
}

fn to_json_at(v: &Value, top: bool, depth: usize) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    if depth > 64 {
        return Err("a value nested more than 64 levels deep".into());
    }
    Ok(match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => number(*i as f64).and_then(|_| {
            if i.unsigned_abs() > EXACT as u64 {
                Err(format!(
                    "{i} is too large to pass exactly; integers must be within ±2^53"
                ))
            } else {
                Ok(J::from(*i))
            }
        })?,
        Value::Number(n) => number(*n)?,
        Value::String(s) => J::String(
            s.to_str()
                .map_err(|_| "a string that is not UTF-8 (pass bytes as base64)".to_owned())?
                .to_owned(),
        ),
        Value::Table(t) => {
            let len = t.raw_len();
            let mut count = 0usize;
            let mut keys = Vec::new();
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair.map_err(|e| e.to_string())?;
                count += 1;
                keys.push((k, v));
            }
            if count == 0 {
                if top {
                    J::Object(serde_json::Map::new())
                } else {
                    J::Array(Vec::new())
                }
            } else if len > 0 && count == len {
                let mut items = Vec::with_capacity(len);
                for i in 1..=len {
                    let item: Value = t.raw_get(i).map_err(|e| e.to_string())?;
                    items.push(to_json_at(&item, false, depth + 1)?);
                }
                J::Array(items)
            } else {
                let mut map = serde_json::Map::new();
                for (k, v) in keys {
                    let key = match &k {
                        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_owned(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) if n.fract() == 0.0 && n.abs() <= EXACT => {
                            format!("{}", *n as i64)
                        }
                        other => return Err(format!("a {} key in a record", other.type_name())),
                    };
                    map.insert(key, to_json_at(&v, false, depth + 1)?);
                }
                J::Object(map)
            }
        }
        other => {
            return Err(format!(
                "a {} cannot be sent to the host",
                other.type_name()
            ));
        }
    })
}

fn number(n: f64) -> Result<serde_json::Value, String> {
    if !n.is_finite() {
        return Err(format!("{n} is not a number the API accepts"));
    }
    if n.fract() == 0.0 {
        if n.abs() > EXACT {
            return Err(format!(
                "{n} is too large to pass exactly; integers must be within ±2^53"
            ));
        }
        return Ok(serde_json::Value::from(n as i64));
    }
    serde_json::Number::from_f64(n)
        .map(serde_json::Value::Number)
        .ok_or_else(|| format!("{n} is not a number the API accepts"))
}

/// A JSON value as a Luau value.
pub fn to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Number(i as f64),
            None => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table_with_capacity(0, map.len())?;
            for (k, item) in map {
                t.raw_set(k.as_str(), to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round(lua: &Lua, src: &str) -> Result<serde_json::Value, String> {
        let v: Value = lua.load(src).eval().map_err(|e| e.to_string())?;
        to_json(&v, true)
    }

    #[test]
    fn records_lists_numbers_and_their_limits() {
        let lua = Lua::new();
        assert_eq!(
            round(&lua, "return { pane = 3, history = 0, endpoint = 'x' }").expect("converts"),
            json!({ "pane": 3, "history": 0, "endpoint": "x" })
        );
        assert_eq!(round(&lua, "return {}").expect("converts"), json!({}));
        assert_eq!(
            round(&lua, "return { kinds = {}, panes = {1, 2} }").expect("converts"),
            json!({ "kinds": [], "panes": [1, 2] })
        );
        assert_eq!(
            round(&lua, "return { x = 0.5 }").expect("converts"),
            json!({ "x": 0.5 })
        );
        assert!(
            round(&lua, "return { x = 2^60 }")
                .expect_err("refused")
                .contains("2^53")
        );
        assert!(round(&lua, "return { x = 0/0 }").is_err());
        assert!(round(&lua, "return { f = print }").is_err());
        let back = to_lua(&lua, &json!({ "a": [1, "b", true], "n": null })).expect("converts");
        assert_eq!(
            to_json(&back, true).expect("converts"),
            json!({ "a": [1, "b", true] })
        );
    }
}
