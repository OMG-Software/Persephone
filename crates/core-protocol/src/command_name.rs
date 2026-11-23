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

/// Serializes a tagged enum's value and keeps its `command` tag.
pub(crate) fn tag_of<T: Serialize + ?Sized>(value: &T) -> Option<String> {
    let mut found = None;
    let _ = value.serialize(Outer { found: &mut found });
    found
}

macro_rules! stop {
    ($($method:ident($($arg:ty),*)),* $(,)?) => {
        $(fn $method(self, $(_: $arg),*) -> Result<Self::Ok, Stop> { Err(Stop) })*
    };
}

macro_rules! refuse_compounds {
    () => {
        type SerializeSeq = Impossible<(), Stop>;
        type SerializeTuple = Impossible<(), Stop>;
        type SerializeTupleStruct = Impossible<(), Stop>;
        type SerializeTupleVariant = Impossible<(), Stop>;
        type SerializeMap = Impossible<(), Stop>;
        type SerializeStructVariant = Impossible<(), Stop>;

        stop!(
            serialize_bool(bool),
            serialize_i8(i8),
            serialize_i16(i16),
            serialize_i32(i32),
            serialize_i64(i64),
            serialize_u8(u8),
            serialize_u16(u16),
            serialize_u32(u32),
            serialize_u64(u64),
            serialize_f32(f32),
            serialize_f64(f64),
            serialize_char(char),
            serialize_bytes(&[u8]),
            serialize_none(),
            serialize_unit(),
            serialize_unit_struct(&'static str),
            serialize_unit_variant(&'static str, u32, &'static str),
        );

        fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<(), Stop> {
            Err(Stop)
        }

        fn serialize_newtype_struct<T: Serialize + ?Sized>(
            self,
            _: &'static str,
            _: &T,
        ) -> Result<(), Stop> {
            Err(Stop)
        }

        fn serialize_newtype_variant<T: Serialize + ?Sized>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<(), Stop> {
            Err(Stop)
        }

        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Stop> {
            Err(Stop)
        }

        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Stop> {
            Err(Stop)
        }

        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, Stop> {
            Err(Stop)
        }

        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, Stop> {
            Err(Stop)
        }

        fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Stop> {
            Err(Stop)
        }

        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, Stop> {
            Err(Stop)
        }
    };
}

/// The enum value itself: only a struct is accepted.
struct Outer<'a> {
    found: &'a mut Option<String>,
}

impl<'a> ser::Serializer for Outer<'a> {
    type Ok = ();
    type Error = Stop;
    type SerializeStruct = Fields<'a>;

    refuse_compounds!();

    fn serialize_str(self, _: &str) -> Result<(), Stop> {
        Err(Stop)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Fields<'a>, Stop> {
        Ok(Fields { found: self.found })
    }
}

/// The struct's fields: the first must be the tag.
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
        *self.found = Some(value.serialize(Text)?);
        Ok(())
    }

    fn end(self) -> Result<(), Stop> {
        Ok(())
    }
}

/// The tag's value: only a string is accepted.
struct Text;

impl ser::Serializer for Text {
    type Ok = String;
    type Error = Stop;
    type SerializeStruct = Impossible<String, Stop>;
    type SerializeSeq = Impossible<String, Stop>;
    type SerializeTuple = Impossible<String, Stop>;
    type SerializeTupleStruct = Impossible<String, Stop>;
    type SerializeTupleVariant = Impossible<String, Stop>;
    type SerializeMap = Impossible<String, Stop>;
    type SerializeStructVariant = Impossible<String, Stop>;

    stop!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char),
        serialize_bytes(&[u8]),
        serialize_none(),
        serialize_unit(),
        serialize_unit_struct(&'static str),
        serialize_unit_variant(&'static str, u32, &'static str),
    );

    fn serialize_str(self, value: &str) -> Result<String, Stop> {
        // Tags are short snake_case names.
        if value.len() <= 64 {
            Ok(value.to_owned())
        } else {
            Err(Stop)
        }
    }

    fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<String, Stop> {
        Err(Stop)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<String, Stop> {
        Err(Stop)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<String, Stop> {
        Err(Stop)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Stop> {
        Err(Stop)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Stop> {
        Err(Stop)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Stop> {
        Err(Stop)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Stop> {
        Err(Stop)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Stop> {
        Err(Stop)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Stop> {
        Err(Stop)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Stop> {
        Err(Stop)
    }
}
