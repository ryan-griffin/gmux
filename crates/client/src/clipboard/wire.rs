// Clipboard metadata only. Packet owns binary framing and validation.
use super::*;
use serde::{
    Deserializer, Serializer,
    de::{self, SeqAccess, Visitor},
};

#[derive(Deserialize)]
struct Descriptor {
    mime: String,
    len: usize,
}

#[derive(Serialize)]
struct DescriptorRef<'a> {
    mime: &'a str,
    len: usize,
}

#[derive(Serialize)]
struct WriteHeader<'a> {
    destination: Destination,
    content: Option<DescriptorsRef<'a>>,
}

struct DescriptorsRef<'a>(&'a [Representation]);
impl Serialize for DescriptorsRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for r in self.0 {
            seq.serialize_element(&DescriptorRef {
                mime: &r.mime,
                len: r.data.len(),
            })?;
        }
        seq.end()
    }
}

pub fn serialize<S: Serializer>(write: &Option<Write>, serializer: S) -> Result<S::Ok, S::Error> {
    write
        .as_ref()
        .map(|write| WriteHeader {
            destination: write.destination,
            content: write.content.as_ref().map(|offer| DescriptorsRef(&offer.0)),
        })
        .serialize(serializer)
}

struct Descriptors(Vec<Descriptor>);
impl<'de> Deserialize<'de> for Descriptors {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Bounded;
        impl<'de> Visitor<'de> for Bounded {
            type Value = Descriptors;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a bounded list of clipboard MIME descriptors")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut contents = Vec::<Descriptor>::new();
                let mut total = 0usize;
                while let Some(item) = seq.next_element::<Descriptor>()? {
                    if contents.len() == MAX_REPRESENTATIONS {
                        return Err(de::Error::custom(InvalidOffer::Unsupported));
                    }
                    validate_metadata(&item.mime, item.len).map_err(de::Error::custom)?;
                    total += item.len; // Each length was bounded above.
                    if total > MAX_OFFER_BYTES || contents.iter().any(|r| r.mime == item.mime) {
                        return Err(de::Error::custom(InvalidOffer::InvalidData));
                    }
                    contents.push(item);
                }
                if contents.is_empty() {
                    return Err(de::Error::custom(InvalidOffer::InvalidData));
                }
                Ok(Descriptors(contents))
            }
        }
        deserializer.deserialize_seq(Bounded)
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Write>, D::Error> {
    #[derive(Deserialize)]
    struct Incoming {
        destination: Destination,
        content: Option<Descriptors>,
    }
    Ok(
        Option::<Incoming>::deserialize(deserializer)?.map(|write| Write {
            destination: write.destination,
            content: write.content.map(|d| {
                Offer(
                    d.0.into_iter()
                        .map(|r| Representation {
                            mime: r.mime,
                            data: vec![0; r.len],
                        })
                        .collect(),
                )
            }),
        }),
    )
}
