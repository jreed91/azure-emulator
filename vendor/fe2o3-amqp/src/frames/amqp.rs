//! AMQP frame type and corresponding encoder and decoder

use bytes::{Buf, BufMut, BytesMut};
use fe2o3_amqp_types::performatives::{
    Attach, Begin, Close, Detach, Disposition, End, Flow, Open, Performative, Transfer,
};
use serde::{ser::Serialize, Deserialize};
use serde_amqp::{de::Deserializer, read::IoReader};
use tokio_util::codec::{Decoder, Encoder};

use crate::{link::clear_continuation_fields, Payload};

use super::{Error, FRAME_TYPE_AMQP};

/// AMQP frame
#[derive(Debug)]
pub struct Frame {
    /// AMQP frame channel
    pub channel: u16,

    /// AMQP frame body
    pub body: FrameBody,
}

impl Frame {
    /// Creates a new AMQP frame
    pub fn new(channel: impl Into<u16>, body: FrameBody) -> Self {
        Self {
            channel: channel.into(),
            body,
        }
    }

    /// Get the channel of the frame
    pub fn channel(&self) -> u16 {
        self.channel
    }

    /// Get the body of the frame
    pub fn body(&self) -> &FrameBody {
        &self.body
    }

    /// Consume the frame to get the frame body
    pub fn into_body(self) -> FrameBody {
        self.body
    }

    /// Creates an emtpy frame. The empty frame is only used to reset
    /// the remote idle timeout
    pub fn empty() -> Self {
        Self {
            channel: 0,
            body: FrameBody::Empty,
        }
    }
}

/// Encoder of the AMQP frames
#[derive(Debug)]
pub struct FrameEncoder {
    /// Max frame size for the transport
    max_frame_body_size: usize,
}

fn write_header(dst: &mut BytesMut, channel: u16) {
    // AMQP frame ignores extended header, thus doff should always be 2
    dst.put_u8(2); // doff
    dst.put_u8(FRAME_TYPE_AMQP); // frame type
    dst.put_u16(channel);
}

impl FrameEncoder {
    pub(crate) fn new(max_frame_size: usize) -> Self {
        Self {
            max_frame_body_size: max_frame_size - 4,
        }
    }

    fn encode_transfer(
        &self,
        dst: &mut BytesMut,
        channel: u16,
        mut transfer: Transfer,
        mut payload: Payload,
    ) -> Result<(), serde_amqp::Error> {
        use serde_amqp::ser::Serializer;

        // First test the size
        let mut buf = BytesMut::new();
        let writer = (&mut buf).writer();
        let mut buf_serializer = Serializer::from(writer);
        transfer.serialize(&mut buf_serializer)?;
        let remaining_bytes = buf.len() + payload.len();
        let more = remaining_bytes > self.max_frame_body_size;

        if more {
            // DEFENSIVE PATH — likely never taken.
            //
            // The sender link (`SenderLink::send_transfer_without_modifying_unsettled_map`)
            // already splits payloads with the same bound as this encoder
            // (`max_frame_size - 4 - performative_size`, including worst-case
            // delivery-id headroom), so every link-level transfer produces
            // exactly one physical frame. If this branch ever triggers, it
            // indicates a sizing mismatch (e.g. a new field added to the
            // `Transfer` performative or a code path that bypasses the link
            // split). Note that frames split here are NOT counted by the
            // session (`next-outgoing-id` / `remote-incoming-window` are only
            // updated once per link-level transfer), so this path must stay a
            // safety net rather than the primary splitting mechanism.
            let orig_more = transfer.more; // If the transfer is pre-split at link
            transfer.more = true;
            buf.clear();
            let writer = (&mut buf).writer();
            let mut serializer = Serializer::from(writer);
            transfer.serialize(&mut serializer)?;
            let split_index = self.max_frame_body_size - buf.len();

            // Send first frame
            let partial = payload.split_to(split_index);
            write_header(dst, channel);
            dst.put(&buf[..]);
            dst.put(partial);

            // Send middle frames
            clear_continuation_fields(&mut transfer);
            buf.clear();
            let writer = (&mut buf).writer();
            let mut serializer = Serializer::from(writer);
            transfer.serialize(&mut serializer)?;

            let mut remaining_bytes = buf.len() + payload.len();
            let split_index = self.max_frame_body_size - buf.len();

            while remaining_bytes > self.max_frame_body_size {
                // The transfer performative can be kept the same for the first n-1 frames
                let partial = payload.split_to(split_index);
                write_header(dst, channel);
                dst.put(&buf[..]);
                dst.put(partial);

                remaining_bytes = buf.len() + payload.len();
            }

            // Send last frame
            transfer.more = orig_more;
            buf.clear();
            let writer = (&mut buf).writer();
            let mut serializer = Serializer::from(writer);
            transfer.serialize(&mut serializer)?;

            write_header(dst, channel);
            dst.put(buf);
            dst.put(payload);
        } else {
            write_header(dst, channel);
            dst.put(buf);
            dst.put(payload);
        }

        Ok(())
    }
}

impl Encoder<Frame> for FrameEncoder {
    type Error = Error;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item.body {
            FrameBody::Open(performative) => write_padded(dst, item.channel, &performative, OPEN_FIELDS),
            FrameBody::Begin(performative) => write_padded(dst, item.channel, &performative, BEGIN_FIELDS),
            FrameBody::Attach(performative) => write_padded(dst, item.channel, &performative, ATTACH_FIELDS),
            FrameBody::Flow(performative) => write_padded(dst, item.channel, &performative, FLOW_FIELDS),
            FrameBody::Transfer {
                performative,
                payload,
            } => self.encode_transfer(dst, item.channel, performative, payload),
            FrameBody::Disposition(performative) => write_padded(dst, item.channel, &performative, DISPOSITION_FIELDS),
            FrameBody::Detach(performative) => write_padded(dst, item.channel, &performative, DETACH_FIELDS),
            FrameBody::End(performative) => write_padded(dst, item.channel, &performative, &[None]),
            FrameBody::Close(performative) => write_padded(dst, item.channel, &performative, &[None]),
            FrameBody::Empty => {
                write_header(dst, item.channel);
                Ok(())
            }
        }
        .map_err(Into::into)
    }
}

/// A field's default value, per the specification.
enum Default {
    Uint(u32),
    Ushort(u16),
    Ubyte(u8),
    Bool(bool),
}

use Default::{Bool, Ubyte, Uint, Ushort};

const OPEN_FIELDS: &[Option<Default>] = &[None, None, Some(Uint(u32::MAX)), Some(Ushort(u16::MAX)), None, None, None, None, None, None];
const BEGIN_FIELDS: &[Option<Default>] = &[None, None, None, None, Some(Uint(u32::MAX)), None, None, None];
const ATTACH_FIELDS: &[Option<Default>] =
    &[None, None, None, Some(Ubyte(2)), Some(Ubyte(0)), None, None, None, Some(Bool(false)), None, None, None, None, None];
const FLOW_FIELDS: &[Option<Default>] = &[None, None, None, None, None, None, None, None, Some(Bool(false)), Some(Bool(false)), None];
const DISPOSITION_FIELDS: &[Option<Default>] = &[None, None, None, Some(Bool(false)), None, Some(Bool(false))];
const DETACH_FIELDS: &[Option<Default>] = &[None, Some(Bool(false)), None];

/// Writes a performative with all of its `fields`, the trailing empty ones
/// as nulls. (Patched in for the azure-emulator: the derived encoding drops
/// trailing empty fields, which the specification allows but some clients,
/// such as the AMQP client inside the Python Event Hubs SDK, index into
/// without checking the list's length.)
fn write_padded<T: Serialize>(dst: &mut BytesMut, channel: u16, performative: &T, fields: &[Option<Default>]) -> Result<(), serde_amqp::Error> {
    use serde_amqp::Value;

    let bytes = serde_amqp::to_vec(performative)?;
    let mut value: Value = serde_amqp::from_slice(&bytes)?;
    if let Value::Described(described) = &mut value {
        if let Value::List(items) = &mut described.value {
            while items.len() < fields.len() {
                items.push(Value::Null);
            }
            // A field left out for having its default value is written with
            // that value: a null would read as "no value" to such clients.
            for (item, default) in items.iter_mut().zip(fields) {
                if let (Value::Null, Some(default)) = (&item, default) {
                    *item = match default {
                        Default::Uint(n) => Value::Uint(*n),
                        Default::Ushort(n) => Value::Ushort(*n),
                        Default::Ubyte(n) => Value::Ubyte(*n),
                        Default::Bool(b) => Value::Bool(*b),
                    };
                }
            }
        }
    }
    write_header(dst, channel);
    dst.extend_from_slice(&serde_amqp::to_vec(&value)?);
    Ok(())
}

/// Decoder of the AMQP frames
#[derive(Debug)]
pub struct FrameDecoder {}

impl Decoder for FrameDecoder {
    type Item = Frame;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let doff = src.get_u8();
        let ftype = src.get_u8();
        let channel = src.get_u16();

        // check type byte
        if ftype != FRAME_TYPE_AMQP {
            return Err(Error::NotImplemented);
        }

        match doff {
            2 => {}
            _ => return Err(Error::NotImplemented),
        }

        let body = if src.is_empty() {
            FrameBody::Empty
        } else {
            // A frame other than a transfer is the performative alone, so it
            // can be decoded again leniently if the strict decoding fails.
            let is_transfer = src.len() >= 3 && src[..3] == [0x00, 0x53, 0x14];
            let copy = (!is_transfer).then(|| src.clone());
            let reader = IoReader::new(src.reader());
            let mut deserializer = Deserializer::new(reader);
            let performative: Performative = match Deserialize::deserialize(&mut deserializer) {
                Ok(performative) => performative,
                Err(error) => match copy.and_then(|bytes| lenient_performative(&bytes)) {
                    Some(performative) => performative,
                    None => return Err(error.into()),
                },
            };

            match performative {
                Performative::Open(performative) => FrameBody::Open(performative),
                Performative::Begin(performative) => FrameBody::Begin(performative),
                Performative::Attach(performative) => FrameBody::Attach(performative),
                Performative::Transfer(performative) => {
                    let payload = src.split().into();
                    FrameBody::Transfer {
                        performative,
                        payload,
                    }
                }
                Performative::Flow(performative) => FrameBody::Flow(performative),
                Performative::Disposition(performative) => FrameBody::Disposition(performative),
                Performative::Detach(performative) => FrameBody::Detach(performative),
                Performative::End(performative) => FrameBody::End(performative),
                Performative::Close(performative) => FrameBody::Close(performative),
            }
        };

        Ok(Some(Frame { channel, body }))
    }
}

/// Decodes a performative whose maps have string keys where the
/// specification asks for symbols (the `info` of an error, say), as some
/// clients send them: the keys are made symbols and the frame decoded again.
/// (Patched in for the azure-emulator: the rhea client behind the JavaScript
/// Service Bus SDK dead-letters with string-keyed error info.)
fn lenient_performative(bytes: &[u8]) -> Option<Performative> {
    use serde_amqp::primitives::Symbol;
    use serde_amqp::Value;

    fn symbolize(value: Value) -> Value {
        match value {
            Value::Map(map) => Value::Map(
                map.into_iter()
                    .map(|(key, value)| {
                        let key = match key {
                            Value::String(key) => Value::Symbol(Symbol::from(key)),
                            other => other,
                        };
                        (key, symbolize(value))
                    })
                    .collect(),
            ),
            Value::List(items) => Value::List(items.into_iter().map(symbolize).collect()),
            Value::Described(mut described) => {
                described.value = symbolize(described.value);
                Value::Described(described)
            }
            other => other,
        }
    }

    let value: Value = serde_amqp::from_slice(bytes).ok()?;
    let bytes = serde_amqp::to_vec(&symbolize(value)).ok()?;
    serde_amqp::from_slice(&bytes).ok()
}

/// AMQP frame body
// #[derive(Debug)]
pub enum FrameBody {
    // Frames handled by Link
    /// Attach performative
    Attach(Attach),

    /// Flow performative
    Flow(Flow),

    /// Transfer performative and payload
    Transfer {
        /// Transfer performative
        performative: Transfer,

        /// Binary payload
        payload: Payload, // The payload should have ownership passed around not shared
    },

    /// Disposition performative
    Disposition(Disposition),

    /// Detach performative
    Detach(Detach),

    // Frames handled by Session
    /// Begin performative
    Begin(Begin),

    /// End performative
    End(End),

    // Frames handled by Connection
    /// Open performative
    Open(Open),

    /// Close performative
    Close(Close),

    /// An empty frame used only for resetting idle timeout
    Empty,
}

impl std::fmt::Debug for FrameBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attach(arg0) => f.debug_tuple("Attach").field(arg0).finish(),
            Self::Flow(arg0) => f.debug_tuple("Flow").field(arg0).finish(),
            Self::Transfer {
                performative,
                payload,
            } => f
                .debug_struct("Transfer")
                .field("performative", performative)
                .field("payload.len", &payload.len())
                .finish(),
            Self::Disposition(arg0) => f.debug_tuple("Disposition").field(arg0).finish(),
            Self::Detach(arg0) => f.debug_tuple("Detach").field(arg0).finish(),
            Self::Begin(arg0) => f.debug_tuple("Begin").field(arg0).finish(),
            Self::End(arg0) => f.debug_tuple("End").field(arg0).finish(),
            Self::Open(arg0) => f.debug_tuple("Open").field(arg0).finish(),
            Self::Close(arg0) => f.debug_tuple("Close").field(arg0).finish(),
            Self::Empty => write!(f, "Empty"),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    use crate::frames::amqp::{FrameDecoder, FrameEncoder};

    use super::Frame;

    #[test]
    fn test_encoding_empty_frame() {
        let empty = Frame::empty();
        let mut encoder = FrameEncoder::new(512);
        let mut dst = BytesMut::new();
        encoder.encode(empty, &mut dst).unwrap();
    }

    #[test]
    fn test_decode_empty_frame() {
        let mut decoder = FrameDecoder {};
        let mut src = BytesMut::from(&[0x02, 0x00, 0x00, 0x00][..]);
        let _frame = decoder.decode(&mut src).unwrap();
    }
}
