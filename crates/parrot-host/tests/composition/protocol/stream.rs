use parrot_domain::{contracts::async_ops::Cancellation, protocol::messages::parrot::Event};
use parrot_host::composition::protocol::{
    RpcCode, SessionEventStream, stream::bounded_client_stream,
};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::events::NeverCancelled;
use protobuf::Message;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn bounded_source_channel_blocks_the_fifth_write_until_a_read() {
    let (writer, mut reader) = bounded_client_stream();
    for value in 0..4 {
        writer.write(value, &NeverCancelled).await.unwrap();
    }
    let fifth = writer.write(4, &NeverCancelled);
    tokio::pin!(fifth);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), fifth.as_mut())
            .await
            .is_err()
    );
    assert_eq!(reader.next(&NeverCancelled).await.unwrap(), Some(0));
    fifth.await.unwrap();
    writer.stop();
    for value in 1..5 {
        assert_eq!(reader.next(&NeverCancelled).await.unwrap(), Some(value));
    }
    assert_eq!(reader.next(&NeverCancelled).await.unwrap(), None);
}

#[tokio::test]
async fn stopping_rejects_pending_and_later_writes_but_drains_buffered_messages() {
    let (writer, mut reader) = bounded_client_stream();
    for value in 0..4 {
        writer.write(value, &NeverCancelled).await.unwrap();
    }
    let fifth = writer.write(4, &NeverCancelled);
    tokio::pin!(fifth);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), fifth.as_mut())
            .await
            .is_err()
    );
    writer.stop();
    writer.stop();
    assert_eq!(fifth.await.unwrap_err().code, RpcCode::Internal);
    assert_eq!(
        writer.write(5, &NeverCancelled).await.unwrap_err().code,
        RpcCode::Internal
    );
    for value in 0..4 {
        assert_eq!(reader.next(&NeverCancelled).await.unwrap(), Some(value));
    }
    assert_eq!(reader.next(&NeverCancelled).await.unwrap(), None);
}

#[tokio::test]
async fn stop_completes_the_reader_without_repolling_a_backpressured_write() {
    let (writer, mut reader) = bounded_client_stream();
    for value in 0..4 {
        writer.write(value, &NeverCancelled).await.unwrap();
    }
    let fifth = writer.write(4, &NeverCancelled);
    tokio::pin!(fifth);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), fifth.as_mut())
            .await
            .is_err()
    );
    writer.stop();
    for value in 0..4 {
        assert_eq!(reader.next(&NeverCancelled).await.unwrap(), Some(value));
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reader.next(&NeverCancelled))
            .await
            .expect("stop must complete the reader independently of parked writers")
            .unwrap(),
        None
    );
    assert_eq!(fifth.await.unwrap_err().code, RpcCode::Internal);
}

#[tokio::test]
async fn cancellation_does_not_enqueue_or_consume_an_item() {
    let (writer, mut reader) = bounded_client_stream();
    let cancelled = Signal::new();
    cancelled.cancel();
    assert!(cancelled.is_cancelled());
    assert_eq!(
        writer.write(1, &cancelled).await.unwrap_err().code,
        RpcCode::Cancelled
    );
    writer.write(2, &NeverCancelled).await.unwrap();
    assert_eq!(
        reader.next(&cancelled).await.unwrap_err().code,
        RpcCode::Cancelled
    );
    assert_eq!(reader.next(&NeverCancelled).await.unwrap(), Some(2));
    writer.stop();
    assert_eq!(reader.next(&NeverCancelled).await.unwrap(), None);
}

#[tokio::test]
async fn dropped_receiver_releases_backpressured_writer() {
    let (writer, reader) = bounded_client_stream();
    for value in 0..4 {
        writer.write(value, &NeverCancelled).await.unwrap();
    }
    let writer = Arc::new(writer);
    let running = writer.clone();
    let pending = tokio::spawn(async move { running.write(4, &NeverCancelled).await });
    tokio::task::yield_now().await;
    drop(reader);
    assert_eq!(pending.await.unwrap().unwrap_err().code, RpcCode::Internal);
}

#[tokio::test]
async fn typed_event_stream_preserves_unknown_wire_fields_and_terminal_events_do_not_end_it() {
    let (writer, mut reader) = bounded_client_stream::<Event>();
    let mut terminal = Event::new();
    terminal.id = "terminal".into();
    terminal.set_turn_ended(Default::default());
    terminal
        .special_fields
        .mut_unknown_fields()
        .add_varint(499, 18446744073709551615);
    terminal
        .special_fields
        .mut_unknown_fields()
        .add_length_delimited(500, vec![0, 255, 0]);
    let wire = terminal.write_to_bytes().unwrap();
    writer.write(terminal, &NeverCancelled).await.unwrap();
    let received = SessionEventStream::next(&mut reader, &NeverCancelled)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.write_to_bytes().unwrap(), wire);
    {
        let next = SessionEventStream::next(&mut reader, &NeverCancelled);
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), next.as_mut())
                .await
                .is_err()
        );
        let later = Event {
            id: "child-after-terminal".into(),
            ..Event::new()
        };
        writer.write(later, &NeverCancelled).await.unwrap();
        assert_eq!(next.await.unwrap().unwrap().id, "child-after-terminal");
    }
    writer.stop();
    assert!(
        SessionEventStream::next(&mut reader, &NeverCancelled)
            .await
            .unwrap()
            .is_none()
    );
}
