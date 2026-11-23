//! A command's name without its arguments, for the diagnostic log
//! (ADR-0058).
//!
//! The derived `Serialize` of the internally tagged [`Call`](crate::Call)
//! writes the `command` tag first. This serializer keeps only that tag and
//! stops with an error at the next field, so no argument, such as a
//! password, is ever copied.

use serde::ser::{self, Impossible, Serialize};
use std::fmt;

/// Why the name serializer stopped. Never reaches a caller.
#[derive(Debug)]
pub(crate) struct Stop;

impl fmt::Display for Stop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("stop")
    }
}

impl std::error::Error for Stop {}

impl ser::Error for Stop {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self
    }
}

/// Tags are short snake_case names.
const MAX_TAG_BYTES: usize = 64;

/// Serializes a tagged enum's value and keeps its `command` tag.
pub(crate) fn tag_of<T: Serialize + ?Sized>(value: &T) -> Option<String> {
    let mut found = None;
    let _ = value.serialize(Probe {
        found: &mut found,
        tag: false,
    });
    found
}

/// One serializer for both levels: the enum value, which must be a struct,
/// and the tag's value (`tag`), which must be a short string.
struct Probe<'a> {
    found: &'a mut Option<String>,
    tag: bool,
}

/// Every other shape stops the serializer.
macro_rules! stop {
    ($($method:ident $(<$generic:ident>)? ($($arg:ty),*) -> $ret:ty;)*) => {
        $(fn $method $(<$generic: Serialize + ?Sized>)? (self, $(_: $arg),*) -> Result<$ret, Stop> { Err(Stop) })*
    };
}

impl<'a> ser::Serializer for Probe<'a> {
    type Ok = ();
    type Error = Stop;
    type SerializeStruct = Fields<'a>;
    type SerializeSeq = Impossible<(), Stop>;
    type SerializeTuple = Impossible<(), Stop>;
    type SerializeTupleStruct = Impossible<(), Stop>;
    type SerializeTupleVariant = Impossible<(), Stop>;
    type SerializeMap = Impossible<(), Stop>;
    type SerializeStructVariant = Impossible<(), Stop>;

    stop! {
        serialize_bool(bool) -> (); serialize_i8(i8) -> (); serialize_i16(i16) -> ();
        serialize_i32(i32) -> (); serialize_i64(i64) -> (); serialize_u8(u8) -> ();
        serialize_u16(u16) -> (); serialize_u32(u32) -> (); serialize_u64(u64) -> ();
        serialize_f32(f32) -> (); serialize_f64(f64) -> (); serialize_char(char) -> ();
        serialize_bytes(&[u8]) -> (); serialize_none() -> (); serialize_unit() -> ();
        serialize_unit_struct(&'static str) -> ();
        serialize_unit_variant(&'static str, u32, &'static str) -> ();
        serialize_some<T>(&T) -> ();
        serialize_newtype_struct<T>(&'static str, &T) -> ();
        serialize_newtype_variant<T>(&'static str, u32, &'static str, &T) -> ();
        serialize_seq(Option<usize>) -> Self::SerializeSeq;
        serialize_tuple(usize) -> Self::SerializeTuple;
        serialize_tuple_struct(&'static str, usize) -> Self::SerializeTupleStruct;
        serialize_tuple_variant(&'static str, u32, &'static str, usize) -> Self::SerializeTupleVariant;
        serialize_map(Option<usize>) -> Self::SerializeMap;
        serialize_struct_variant(&'static str, u32, &'static str, usize) -> Self::SerializeStructVariant;
    }

    fn serialize_str(self, value: &str) -> Result<(), Stop> {
        if !self.tag || value.len() > MAX_TAG_BYTES {
            return Err(Stop);
        }
        *self.found = Some(value.to_owned());
        Ok(())
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Fields<'a>, Stop> {
        if self.tag {
            return Err(Stop);
        }
        Ok(Fields { found: self.found })
    }
}

/// The struct's fields: the first must be the tag; any field after it stops.
struct Fields<'a> {
    found: &'a mut Option<String>,
}

impl ser::SerializeStruct for Fields<'_> {
    type Ok = ();
    type Error = Stop;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Stop> {
        if key != "command" || self.found.is_some() {
            return Err(Stop);
        }
        value.serialize(Probe {
            found: self.found,
            tag: true,
        })
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::tag_of;
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(tag = "command", rename_all = "snake_case")]
    enum Tagged {
        SignIn { password: String },
        Plain {},
    }

    #[derive(Serialize)]
    struct Untagged {
        password: String,
        command: String,
    }

    #[derive(Serialize)]
    struct TextTag {
        command: String,
    }

    #[derive(Serialize)]
    struct NumberTag {
        command: u8,
    }

    #[derive(Serialize)]
    struct NestedTag {
        command: NumberTag,
    }

    /// GIVEN a tagged value with an argument THEN only the tag is kept, and
    /// a value without arguments keeps its tag too.
    #[test]
    fn given_tagged_value_then_only_the_tag_is_kept() {
        let call = Tagged::SignIn {
            password: "hunter2".into(),
        };
        assert_eq!(tag_of(&call).as_deref(), Some("sign_in"));
        assert_eq!(tag_of(&Tagged::Plain {}).as_deref(), Some("plain"));
    }

    /// GIVEN values of any other shape THEN no name is found: plain values,
    /// a struct whose first field is not the tag, a tag that is not a string
    /// or is too long.
    #[test]
    fn given_other_shapes_then_no_name() {
        assert_eq!(tag_of(&5u8), None);
        assert_eq!(tag_of("sign_in"), None);
        assert_eq!(tag_of(&vec![1, 2]), None);
        assert_eq!(tag_of(&Some(true)), None);
        assert_eq!(tag_of(&()), None);
        let untagged = Untagged {
            password: "hunter2".into(),
            command: "sign_in".into(),
        };
        assert_eq!(tag_of(&untagged), None);
        assert_eq!(tag_of(&NumberTag { command: 1 }), None);
        assert_eq!(
            tag_of(&NestedTag {
                command: NumberTag { command: 1 }
            }),
            None
        );
        let fits = TextTag {
            command: "x".repeat(64),
        };
        assert_eq!(tag_of(&fits).map(|tag| tag.len()), Some(64));
        let long = TextTag {
            command: "x".repeat(65),
        };
        assert_eq!(tag_of(&long), None);
    }
}
