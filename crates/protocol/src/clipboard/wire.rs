// Clipboard metadata only. Packet owns binary framing and validation.
use super::*;
use serde::{
    Deserializer, Serializer,
    de::{self, DeserializeSeed, SeqAccess, Visitor},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

pub(crate) struct WriteRef<'a>(pub &'a Write);
impl Serialize for WriteRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WriteHeader {
            destination: self.0.destination,
            content: self
                .0
                .content
                .as_ref()
                .map(|offer| DescriptorsRef(&offer.0)),
        }
        .serialize(serializer)
    }
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

/// Validate framed payload length before allocating binary representation data.
pub(crate) struct WriteSeed {
    pub payload: usize,
}
impl<'de> DeserializeSeed<'de> for WriteSeed {
    type Value = Write;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Incoming {
            destination: Destination,
            content: Option<Descriptors>,
        }
        let write = Incoming::deserialize(deserializer)?;
        let declared = write
            .content
            .as_ref()
            .map_or(0, |d| d.0.iter().map(|r| r.len).sum());
        if declared != self.payload {
            return Err(de::Error::custom("Invalid clipboard payload length"));
        }
        Ok(Write {
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
        })
    }
}
