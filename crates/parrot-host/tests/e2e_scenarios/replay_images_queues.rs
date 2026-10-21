use super::*;
use parrot_storage::queue_files::Direction;

#[tokio::test]
async fn long_history_listener_reconnect_image_identity_and_closed_queue_survive_resume() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let id = UserSessionId::parse(&user.id).unwrap();
    let image = bounded(hosted.persist_image(image_upload(), &NeverCancelled))
        .await
        .unwrap();
    bounded(hosted.send(
        vec![
            ConversationPart::text("t76-long-turn-00"),
            ConversationPart::image(&image.artifact_id, &image.media_type, &image.display_name),
        ],
        "t76-long-input-00".into(),
        Delivery::DELIVERY_STEER,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    idle(&root).await;
    for index in 1..32 {
        send(
            &hosted,
            &format!("t76-long-turn-{index:02}"),
            &format!("t76-long-input-{index:02}"),
        )
        .await;
        idle(&root).await;
    }
    let before = user.lease.events.replay().await.unwrap();
    let mut stream = hosted
        .listen(Some(String::new()), Arc::new(NeverCancelled))
        .unwrap();
    let split = before.len() / 2;
    let mut prefix = Vec::new();
    for _ in 0..split {
        prefix.push(
            bounded(stream.next(&NeverCancelled))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let cursor = prefix.last().unwrap().id.clone();
    drop(stream);
    let queues = root.get_service::<AgentQueues<Security>>().unwrap();
    queues
        .create("resume-work", "durable root queue across host reopen")
        .unwrap();
    queues
        .push(
            "resume-work",
            &["first".into(), "second".into()],
            Direction::Back,
            true,
            &NeverCancelled,
        )
        .unwrap();
    let old_queue_path = queues.get("resume-work").unwrap().path;
    let selected = user.resources.root().to_owned();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);

    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let resumed_root = fixture.root();
    let resumed_user = fixture.user();
    let resumed_queues = resumed_root.get_service::<AgentQueues<Security>>().unwrap();
    let queue_before_drain = resumed_queues.get("resume-work").unwrap();
    let drained = resumed_queues
        .try_take("resume-work", 2, Direction::Front)
        .unwrap();
    let exhausted = resumed_queues
        .try_take("resume-work", 1, Direction::Front)
        .unwrap();
    let resolved = bounded(resumed.resolve_image(&image.artifact_id))
        .await
        .unwrap();
    for index in 32..64 {
        send(
            &resumed,
            &format!("t76-long-turn-{index:02}"),
            &format!("t76-long-input-{index:02}"),
        )
        .await;
        idle(&resumed_root).await;
    }
    let after = resumed_user.lease.events.replay().await.unwrap();
    let history = resumed_root.history().conversation().await.unwrap();
    let mut reconnected = resumed
        .listen(Some(cursor), Arc::new(NeverCancelled))
        .unwrap();
    let mut suffix = Vec::new();
    for _ in split..after.len() {
        suffix.push(
            bounded(reconnected.next(&NeverCancelled))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    drop(reconnected);
    let image_paths: Vec<_> = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| {
            request
                .messages
                .iter()
                .flat_map(|message| message.contents())
                .find_map(|content| {
                    (!content.image_path().is_empty()).then(|| content.image_path().to_owned())
                })
        })
        .collect();
    let resumed_selected = resumed_user.resources.root().to_owned();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    assert_eq!(prefix, before[..split]);
    assert_eq!(suffix, after[split..]);
    assert_eq!(&after[..before.len()], before.as_slice());
    assert_eq!(
        after.iter().filter(|event| event.has_turn_ended()).count(),
        64
    );
    let retained_inputs: Vec<_> = history
        .iter()
        .filter_map(|item| {
            let text = parrot_storage::events::content(&item.message.parts);
            text.starts_with("t76-long-turn-").then_some(text)
        })
        .collect();
    assert_eq!(
        retained_inputs,
        (0..64)
            .map(|index| format!("t76-long-turn-{index:02}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(resolved, Some(image));
    assert_eq!(image_paths.len(), 64);
    assert!(image_paths.iter().all(|path| path == &image_paths[0]));
    assert!(Path::new(&image_paths[0]).starts_with(selected.join("artifacts")));
    assert_eq!(selected, resumed_selected);
    assert!(selected.starts_with(fixture.root.join("new/sessions")));
    assert_eq!(queue_before_drain.path, old_queue_path);
    assert!(old_queue_path.starts_with(selected.join("queues")));
    assert_eq!(
        (queue_before_drain.size, queue_before_drain.closed),
        (2, true)
    );
    assert_eq!(drained.items, vec!["first", "second"]);
    assert!(exhausted.items.is_empty());
    assert!(exhausted.info.unwrap().closed);
    assert!(queues.is_disposed());
    assert!(resumed_queues.is_disposed());
}
