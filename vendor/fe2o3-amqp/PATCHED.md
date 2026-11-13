# Patched fe2o3-amqp 0.18.4

This is fe2o3-amqp 0.18.4 (MIT or Apache-2.0, https://github.com/minghuaw/fe2o3-amqp)
with the changes below. The first: `Sendable` (and its builder) carry an optional `delivery_tag`,
which a sender uses instead of a tag generated from its delivery count. Credit
is consumed exactly as before.

Why: Azure Service Bus clients read a peek-locked message's lock token from its
delivery tag as a 16-byte UUID, so the broker must choose the tag. The library
otherwise always generates 4-byte tags.

Files changed: `src/link/delivery.rs`, `src/link/sender.rs`,
`src/link/sender_link.rs`, `src/endpoint/link.rs`. The tests, examples and
benches of the published crate are not included. Drop this patch once upstream
offers the same.

- `DeliveryInfo::settle_on_disposition()` (src/link/delivery.rs): marks a
  received delivery to be settled by the disposition the receiver sends, even
  on a link that negotiated `rcv-settle-mode = second`. Service Bus settles
  every delivery it accepts in one step and the SDKs wait for that.

- `consecutive_chunk_indices` (src/session/mod.rs) now includes the end of the
  last run, so the settled disposition a sender owes a `second`-mode receiver
  is actually sent (before, a single unsettled disposition was never echoed and
  the receiver waited forever). Echoed deliveries are also forgotten.

- `Sender::credit_available()` (src/link/sender.rs): waits until the receiver
  has granted credit, without consuming it, so the broker chooses which
  message to send only when it can be sent at once.

- The settled disposition a sender echoes for a `rejected` outcome carries no
  error (src/session/mod.rs): the receiver's error describes why it rejected,
  and echoed back it reads as a failed settlement to the Service Bus SDKs.

- `Sender::send_encoded()` (src/link/sender.rs): sends a message the caller
  encoded itself. The broker writes the header section by hand so that
  `delivery-count: 0` is on the wire (the derived encoding leaves default
  fields out, and the Python SDK then reports the count as unset).
