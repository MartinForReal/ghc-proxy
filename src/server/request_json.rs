use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;

// The wire-size limit alone doesn't bound the size of the resulting Value tree:
// a short scalar can occupy considerably more space in memory than on the wire.
const MAX_VALUES: usize = 262_144;

pub(super) fn parse_object(input: &[u8]) -> serde_json::Result<Value> {
    parse_with_limit(input, MAX_VALUES)
}

fn parse_with_limit(input: &[u8], limit: usize) -> serde_json::Result<Value> {
    let mut parser = serde_json::Deserializer::from_slice(input);
    let mut remaining = limit;
    let object = parser.deserialize_map(BoundedValue(&mut remaining))?;
    parser.end()?;
    Ok(object)
}

// Charge each field value and array element before parsing it. A single budget
// covers all containers, including repeated keys, rather than resetting per list.
struct BoundedValue<'a>(&'a mut usize);

impl<'de> DeserializeSeed<'de> for BoundedValue<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, parser: D) -> Result<Value, D::Error> {
        *self.0 = self
            .0
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("too many JSON values"))?;
        parser.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for BoundedValue<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = fields.next_key::<String>()? {
            object.insert(key, fields.next_value_seed(BoundedValue(self.0))?);
        }
        Ok(Value::Object(object))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Value, A::Error> {
        let mut array = Vec::new();
        while let Some(value) = items.next_element_seed(BoundedValue(self.0))? {
            array.push(value);
        }
        Ok(Value::Array(array))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(v.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_one_shared_budget_across_nested_containers_and_duplicate_keys() {
        let input = br#"{"a":[true,{"b":null}],"a":2}"#;
        assert_eq!(
            parse_with_limit(input, 5).unwrap(),
            serde_json::json!({"a": 2})
        );
        assert!(parse_with_limit(input, 4).is_err());
    }

    #[test]
    fn preserves_provider_json_and_rejects_incomplete_or_trailing_input() {
        let input = br#"{"extra":["a\nb",-4,18446744073709551615,1.25,false,{},[]]}"#;
        let expected: Value = serde_json::from_slice(input).unwrap();
        assert_eq!(parse_object(input).unwrap(), expected);
        for invalid in [b"[]".as_slice(), b"null", br#"{"a":}"#, b"{} []"] {
            assert!(parse_object(invalid).is_err());
        }
    }
}
