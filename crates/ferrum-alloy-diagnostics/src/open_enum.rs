//! Forward-compatible string enums.
//!
//! Report producers may be newer than this reader. A value this version does not
//! know is preserved as `Unrecognized(String)` and is never mapped onto a known
//! variant, so rules cannot silently treat an unrecognized value as a known one.

/// Declares a string-valued enum with an `Unrecognized(String)` catch-all variant.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A value this reader does not recognize. It is preserved verbatim.
            Unrecognized(String),
        }

        impl $name {
            /// Every value this version recognizes, in declaration order.
            pub const KNOWN: &'static [&'static str] = &[ $( $text ),+ ];

            /// The wire representation.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $text, )+
                    Self::Unrecognized(other) => other.as_str(),
                }
            }

            /// Parses a wire value, preserving unrecognized values.
            pub fn parse(value: &str) -> Self {
                match value {
                    $( $text => Self::$variant, )+
                    other => Self::Unrecognized(other.to_owned()),
                }
            }

            /// Returns `true` when the value was not recognized by this reader.
            pub fn is_unrecognized(&self) -> bool {
                matches!(self, Self::Unrecognized(_))
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <String as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(Self::parse(&value))
            }
        }
    };
}

pub(crate) use open_enum;
