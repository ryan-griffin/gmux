//! One header schema for serialization and deserialization with framed copy limits.
pub(super) struct HeaderSeed {
    pub copy_payload: Option<usize>,
}

// Keep the field list in transport.rs as the single schema. Only `copy` needs a
// seed: its numeric lengths must match the frame before allocating payloads.
macro_rules! packet_header {
    ($( $(#[$attr:meta])* pub $field:ident: $ty:ty, )* ;
        $(#[$copy_attr:meta])* pub copy: $copy_ty:ty,) => {
        #[derive(Default, Serialize)]
        pub struct Header {
            $($(#[$attr])* pub $field: $ty,)*
            $(#[$copy_attr])* pub copy: $copy_ty,
        }

        impl<'de> serde::Deserialize<'de> for Header {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Self, D::Error> {
                serde::de::DeserializeSeed::deserialize(
                    header::HeaderSeed { copy_payload: None },
                    deserializer,
                )
            }
        }

        impl<'de> serde::de::DeserializeSeed<'de> for header::HeaderSeed {
            type Value = Header;
            fn deserialize<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                // Match identifiers through serde, including escaped field names.
                #[allow(non_camel_case_types)]
                #[derive(Deserialize)]
                #[serde(field_identifier)]
                enum Field {
                    $($field,)*
                    copy,
                    #[serde(other)]
                    Ignore,
                }
                struct Fields(header::HeaderSeed);
                impl<'de> serde::de::Visitor<'de> for Fields {
                    type Value = Header;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("a packet header")
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Header, A::Error> {
                        use serde::de::{Error, IgnoredAny};
                        let mut header = Header::default();
                        let mut seen = [false; Field::Ignore as usize];
                        while let Some(field) = map.next_key::<Field>()? {
                            match field {
                                $(Field::$field => {
                                    if std::mem::replace(&mut seen[Field::$field as usize], true) {
                                        return Err(A::Error::duplicate_field(stringify!($field)));
                                    }
                                    header.$field = map.next_value()?;
                                },)*
                                Field::copy => {
                                    if std::mem::replace(&mut seen[Field::copy as usize], true) {
                                        return Err(A::Error::duplicate_field("copy"));
                                    }
                                    header.copy = map.next_value_seed(
                                        crate::clipboard::wire::WriteSeed {
                                            payload: self.0.copy_payload,
                                        },
                                    )?;
                                },
                                Field::Ignore => { map.next_value::<IgnoredAny>()?; },
                            }
                        }
                        // All remaining fields are Option or have #[serde(default)].
                        for (index, name) in [
                            (Field::meta as usize, "meta"),
                            (Field::scroll_serial as usize, "scroll_serial"),
                            (Field::rows as usize, "rows"),
                            (Field::notifications as usize, "notifications"),
                        ] {
                            if !seen[index] {
                                return Err(A::Error::missing_field(name));
                            }
                        }
                        Ok(header)
                    }
                }
                deserializer.deserialize_map(Fields(self))
            }
        }
    }
}
pub(super) use packet_header;
