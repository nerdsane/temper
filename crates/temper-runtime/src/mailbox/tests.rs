use super::*;

#[derive(Debug)]
struct TestMsg(String);
impl Message for TestMsg {}

#[tokio::test]
async fn test_bounded_mailbox_send_recv() {
    let (tx, mut rx) = mailbox::<TestMsg>(10);
    tx.send(Envelope::Tell(TestMsg("hello".into()))).unwrap();
    let msg = rx.recv().await.unwrap();
    match msg {
        Envelope::Tell(TestMsg(s)) => assert_eq!(s, "hello"),
        _ => panic!("expected Tell"),
    }
}

#[tokio::test]
async fn test_bounded_mailbox_full() {
    let (tx, _rx) = mailbox::<TestMsg>(2);
    tx.send(Envelope::Tell(TestMsg("1".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("2".into()))).unwrap();
    let result = tx.send(Envelope::Tell(TestMsg("3".into())));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), ActorError::MailboxFull);
}

#[tokio::test]
async fn test_mailbox_fifo_ordering() {
    let (tx, mut rx) = mailbox::<TestMsg>(10);
    for i in 0..5 {
        tx.send(Envelope::Tell(TestMsg(format!("msg-{i}"))))
            .unwrap();
    }
    for i in 0..5 {
        match rx.recv().await.unwrap() {
            Envelope::Tell(TestMsg(s)) => assert_eq!(s, format!("msg-{i}")),
            _ => panic!("expected Tell"),
        }
    }
}

#[tokio::test]
async fn test_mailbox_sender_clone() {
    let (tx1, mut rx) = mailbox::<TestMsg>(10);
    let tx2 = tx1.clone();
    tx1.send(Envelope::Tell(TestMsg("from-1".into()))).unwrap();
    tx2.send(Envelope::Tell(TestMsg("from-2".into()))).unwrap();

    let m1 = rx.recv().await.unwrap();
    let m2 = rx.recv().await.unwrap();
    match (m1, m2) {
        (Envelope::Tell(TestMsg(a)), Envelope::Tell(TestMsg(b))) => {
            assert_eq!(a, "from-1");
            assert_eq!(b, "from-2");
        }
        _ => panic!("expected Tell"),
    }
}

#[tokio::test]
async fn test_mailbox_closed_on_receiver_drop() {
    let (tx, rx) = mailbox::<TestMsg>(10);
    assert!(!tx.is_closed());
    drop(rx);
    assert!(tx.is_closed());
    let result = tx.send(Envelope::Tell(TestMsg("orphan".into())));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), ActorError::SendFailed);
}

#[tokio::test]
async fn test_mailbox_depth_empty() {
    let (tx, _rx) = mailbox::<TestMsg>(10);
    assert_eq!(tx.depth(), 0);
    assert_eq!(tx.utilization(), 0.0);
}

#[tokio::test]
async fn test_mailbox_depth_after_sends() {
    let (tx, _rx) = mailbox::<TestMsg>(10);
    tx.send(Envelope::Tell(TestMsg("a".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("b".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("c".into()))).unwrap();
    assert_eq!(tx.depth(), 3);
    assert!((tx.utilization() - 0.3).abs() < 0.01);
}

#[tokio::test]
async fn test_mailbox_depth_full() {
    let (tx, _rx) = mailbox::<TestMsg>(3);
    tx.send(Envelope::Tell(TestMsg("1".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("2".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("3".into()))).unwrap();
    assert_eq!(tx.depth(), 3);
    assert_eq!(tx.utilization(), 1.0);
}

#[tokio::test]
async fn test_mailbox_depth_after_recv() {
    let (tx, mut rx) = mailbox::<TestMsg>(10);
    tx.send(Envelope::Tell(TestMsg("a".into()))).unwrap();
    tx.send(Envelope::Tell(TestMsg("b".into()))).unwrap();
    let _ = rx.recv().await;
    assert_eq!(tx.depth(), 1);
}

#[tokio::test]
async fn cancelled_full_mailbox_drain_restores_admission() {
    let (tx, mut rx) = mailbox::<TestMsg>(1);
    tx.send(Envelope::Tell(TestMsg("fills-capacity".into())))
        .expect("the first message fills the bounded mailbox");

    let mut drain = Box::pin(tx.begin_draining(Envelope::Signal(crate::actor::SystemSignal::Stop)));
    tokio::select! {
        biased;
        _ = &mut drain => panic!("the drain barrier entered a full mailbox"),
        () = tokio::task::yield_now() => {}
    }
    assert!(
        tx.is_draining(),
        "the reserving state must fence traffic before barrier capacity exists"
    );
    assert_eq!(
        tx.send(Envelope::Tell(TestMsg(
            "rejected-during-reservation".into()
        ))),
        Err(ActorError::Stopped)
    );

    drop(drain);
    assert!(
        !tx.is_draining(),
        "cancelling before barrier admission restores the accepting state"
    );

    let _ = rx
        .recv()
        .await
        .expect("the original message remains queued");
    tx.send(Envelope::Tell(TestMsg("accepted-after-cancel".into())))
        .expect("traffic is accepted again after drain initiation is cancelled");
}

#[tokio::test]
async fn pending_async_drain_cannot_report_a_false_concurrent_stop_success() {
    let (tx, mut rx) = mailbox::<TestMsg>(1);
    tx.send(Envelope::Tell(TestMsg("fills-capacity".into())))
        .expect("the first message fills the bounded mailbox");

    let mut drain = Box::pin(tx.begin_draining(Envelope::Signal(crate::actor::SystemSignal::Stop)));
    tokio::select! {
        biased;
        _ = &mut drain => panic!("the async drain entered a full mailbox"),
        () = tokio::task::yield_now() => {}
    }
    assert_eq!(
        tx.try_begin_draining(Envelope::Signal(crate::actor::SystemSignal::Stop)),
        Err(ActorError::MailboxFull),
        "a pending async waiter must not make an uncommitted stop look successful"
    );
    drop(drain);

    let _ = rx.recv().await.expect("free capacity after cancellation");
    tx.try_begin_draining(Envelope::Signal(crate::actor::SystemSignal::Stop))
        .expect("the retried synchronous stop commits its barrier");

    assert!(
        tx.is_draining(),
        "a successful stop must retain its admission fence"
    );
    assert_eq!(
        tx.send(Envelope::Tell(TestMsg("late-traffic".into()))),
        Err(ActorError::Stopped)
    );
    assert!(matches!(
        rx.recv().await,
        Some(Envelope::Signal(crate::actor::SystemSignal::Stop))
    ));
}
