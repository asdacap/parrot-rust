# Event delivery and replay

Subscriptions receive live events from the point they are created. Publishing an
event before subscription does not make it available as broker replay. Dropping a
listener releases its buffered events rather than leaving a dead retained queue.
The broker does not persist events or authorize filesystem access.

## Buffering and completion

Each listener has separate lossy capacities for retained and transient events,
with a default of 1,024 events per class. A full class evicts only its oldest
event. Surviving events preserve their interleaved order. Publication never waits
for a subscriber to consume data.

Inventory batches are validated before admission even when no listeners exist.
Pending batches replace earlier pending inventory for the same tool kind and
owner. An active batch finishes contiguously. One ordinary event is preferred
after each batch, preventing continual inventory replenishment from starving it.
Inventory chunks do not consume ordinary-event capacity and are not truncated at
1,024 chunks. Arbitrarily many distinct inventory owners are not globally bounded
by these per-listener capacities.

Explicit broker or listener completion stops new admission and drains buffered
events and inventory chunks. `TurnEnded` does not close a subscription. Canceling
or dropping a single read observation does not close the owned listener.

## Persisted replay and live overlap

A replaying listener subscribes before loading the persisted snapshot so live
events can overlap replay rather than falling into a subscription gap. A missing
cursor skips persisted replay; an explicitly supplied empty cursor does not.
Unknown cursors replay all stored events. Known cursors resume after the cursor's
first occurrence.

Stored events remain unchanged, including duplicate terminal payloads and unknown
protocol fields. Exactly one overlapping live copy is suppressed for each replayed
event identity. Later duplicates are not globally filtered. The live event buffer
remains bounded and lossy during replay.

After the selected persisted suffix has been consumed, the listener captures its
initial scope snapshots and then usage in the supplied order. A slow replay
consumer therefore sees a later initial snapshot rather than one captured when
assembly began. Canceling or dropping an observation does not skip or restart an
initial snapshot already in progress.

Persisted-load failure fails listener creation. Initial-snapshot failure appears
after already visible replay and terminates the listener. Cancellation remains
distinct from either failure. Subsequent live delivery does not start with an
invented successful initial snapshot.

## Retained and durable events

Retained publication is live delivery, not a database commit. A retained event can
be absent from later storage replay. Only events actually committed to storage
can be advertised as committed. Supplying a commit attestation does not itself
persist anything.

Replay preserves duplicate stored terminal events rather than hiding errors in a
turn writer. Correct once-only terminal commitment belongs to the turn's durable
state transition, not to replay filtering. Completing a foreground turn does not
prove that the lifetimes of its child agents have ended.

## Capability limits

The broker's nonblocking lossy fanout is distinct from host RPC backpressure.
The RPC producer queue has four entries and can wait for its consumer; it does
not change broker buffering semantics.

Broker and repository checks do not by themselves establish actual session
factories, child scheduling, host integration, or full runtime acceptance. Scope
and usage snapshots must come from their real owners. Filesystem-security,
native-platform, and release gates remain separate, and known blocked security
work is not waived by event-delivery guarantees.
