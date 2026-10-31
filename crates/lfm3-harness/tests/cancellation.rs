use std::{future::pending, time::Duration};

use lfm3_harness::{CancelReason, CancellationContext};

#[tokio::test]
async fn cancellation_interrupts_active_wait() {
    let context = CancellationContext::new(Duration::from_secs(30));
    let child = context.child();
    let waiter = tokio::spawn(async move { child.cancelled().await });
    context.cancel();
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("cancellation is prompt")
        .expect("waiter task");
}

#[tokio::test]
async fn children_share_parent_cancellation() {
    let context = CancellationContext::new(Duration::from_secs(30));
    let first = context.child();
    let second = context.child();
    assert!(!first.is_cancelled());
    context.cancel();
    assert!(first.is_cancelled());
    assert!(second.is_cancelled());
    assert_eq!(first.cancelled_or_expired().await, CancelReason::Requested);
}

#[tokio::test]
async fn deadline_expires_inside_an_active_wait() {
    let context = CancellationContext::new(Duration::from_millis(50));
    let reason = tokio::time::timeout(
        Duration::from_secs(1),
        context.child().cancelled_or_expired(),
    )
    .await
    .expect("deadline fires before the guard");
    assert_eq!(reason, CancelReason::Deadline);
    assert!(
        !context.is_cancelled(),
        "a deadline is not an explicit cancel"
    );
}

#[tokio::test]
async fn scoped_work_aborts_on_cancel_and_returns_values_otherwise() {
    let context = CancellationContext::new(Duration::from_secs(30));
    let canceller = context.clone();
    let cancelled = context.run(pending::<()>());
    let cancel = tokio::spawn(async move { canceller.cancel() });
    let result = tokio::time::timeout(Duration::from_secs(1), cancelled)
        .await
        .expect("scope aborts promptly");
    cancel.await.unwrap();
    assert_eq!(result.unwrap_err().reason, CancelReason::Requested);

    let context = CancellationContext::new(Duration::from_secs(30));
    assert_eq!(context.run(async { 7_u8 }).await.unwrap(), 7);
}

#[tokio::test]
async fn deadlines_abort_scoped_work() {
    let context = CancellationContext::new(Duration::from_millis(30));
    let outcome = context.run(pending::<()>()).await;
    assert_eq!(outcome.unwrap_err().reason, CancelReason::Deadline);
}

#[tokio::test]
async fn remaining_time_never_exceeds_the_budget() {
    let context = CancellationContext::new(Duration::from_secs(5));
    let remaining = context.remaining();
    assert!(remaining <= Duration::from_secs(5));
    assert!(remaining > Duration::from_secs(4));
    let child_remaining = context.child().remaining();
    assert!(
        child_remaining <= remaining && child_remaining > Duration::from_secs(4),
        "children share the parent's absolute deadline"
    );
}

#[tokio::test]
async fn cancelling_twice_is_idempotent() {
    let context = CancellationContext::new(Duration::from_secs(5));
    context.cancel();
    context.cancel();
    assert!(context.is_cancelled());
}
