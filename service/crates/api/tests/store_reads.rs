//! The bounded reads the turn path uses instead of loading a whole conversation.

mod common;

use common::stack::{Stack, StackOpts};
use pair_core::ids::TraceId;
use pair_core::types::{DataClass, TrustClass};
use pair_models::provider::store::NewMessage;

#[tokio::test]
async fn recent_messages_returns_only_the_newest_in_order_and_find_message_is_targeted() {
    let stack = Stack::start(StackOpts::default()).await;
    let store = &stack.services.store;
    let conversation = store
        .create_conversation_with_class("long", TraceId::new(), DataClass::Public)
        .await
        .expect("conversation");
    for i in 0..30 {
        store
            .append_message(NewMessage {
                conversation,
                client_message_id: format!("m-{i}"),
                role: "user".into(),
                content: format!("message {i}"),
                trust: TrustClass::Owner,
                trace: TraceId::new(),
            })
            .await
            .expect("append");
    }
    let recent = store
        .recent_messages(conversation, 5)
        .await
        .expect("recent");
    let contents: Vec<&str> = recent.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        [
            "message 25",
            "message 26",
            "message 27",
            "message 28",
            "message 29"
        ]
    );
    assert_eq!(
        store
            .recent_messages(conversation, 100)
            .await
            .expect("recent")
            .len(),
        30
    );

    let found = store
        .find_message(conversation, "m-7")
        .await
        .expect("find")
        .expect("exists");
    assert_eq!(found.content, "message 7");
    assert!(store
        .find_message(conversation, "m-404")
        .await
        .expect("find")
        .is_none());
    stack.finish().await;
}
