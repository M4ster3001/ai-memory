//! Rooms (V106).
//!
//! The guards pinned here: a room belongs to one project and a post never
//! reaches another project; only a mention becomes inbox mail, addressed to
//! the mentioned harness, with a fixed body that holds none of the post; a
//! poster never mentions itself; the history is capped; read positions only
//! move forward; and the room cap holds.

use ai_memory_core::rooms::{MAX_ROOMS_PER_PROJECT, ROOM_HISTORY_CAP, RoomName};
use ai_memory_core::{AgentKind, MessageBox, ProjectId, WorkspaceId};
use ai_memory_store::{NewRoomPost, Store};

async fn project(store: &Store, name: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, name.to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

fn post(at: (WorkspaceId, ProjectId), room: &str, from: AgentKind, body: &str) -> NewRoomPost {
    NewRoomPost {
        workspace_id: at.0,
        project_id: at.1,
        room: RoomName::parse(room).unwrap(),
        from_agent: from,
        from_session_id: None,
        from_owner_user: None,
        body: body.into(),
    }
}

async fn inbox(store: &Store, at: (WorkspaceId, ProjectId)) -> Vec<ai_memory_core::AgentMessage> {
    store
        .reader
        .list_messages(at.0, at.1, MessageBox::Inbox, 100)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_post_stays_in_its_room_and_only_a_mention_reaches_an_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let api = project(&store, "api").await;
    let web = project(&store, "web").await;

    let plain = store
        .writer
        .room_post(post(
            api,
            "general",
            AgentKind::ClaudeCode,
            "BODY-CANARY plain",
        ))
        .await
        .unwrap();
    assert_eq!(plain.seq, 1);
    assert!(plain.mentioned.is_empty());
    assert!(
        inbox(&store, api).await.is_empty(),
        "a plain post wakes nobody"
    );

    let asked = store
        .writer
        .room_post(post(
            api,
            "general",
            AgentKind::ClaudeCode,
            "BODY-CANARY @codex can you check? @claude-code is me",
        ))
        .await
        .unwrap();
    assert_eq!(asked.seq, 2);
    assert_eq!(asked.mentioned.len(), 1, "the poster never mentions itself");
    assert_eq!(asked.mentioned[0].0, AgentKind::Codex);
    let mail = inbox(&store, api).await;
    assert_eq!(mail.len(), 1);
    let mention = serde_json::to_value(&mail[0]).unwrap();
    assert_eq!(mention["to_agent"], "codex");
    assert_eq!(mention["from_agent"], "claude-code");
    let text = mention.to_string();
    assert!(
        !text.contains("BODY-CANARY"),
        "post text reached the inbox: {text}"
    );
    assert!(text.contains("post 2"), "{text}");
    assert!(
        inbox(&store, web).await.is_empty(),
        "another project got mail"
    );

    // The same name in another project is another room.
    assert!(store.reader.rooms(web.0, web.1).await.unwrap().is_empty());
    let rooms = store.reader.rooms(api.0, api.1).await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].last_seq, 2);
    let members: Vec<_> = rooms[0].members.iter().map(|m| m.agent).collect();
    assert_eq!(
        members,
        [AgentKind::ClaudeCode],
        "posting joined only the poster"
    );
    assert_eq!(
        rooms[0].members[0].read_seq, 2,
        "a poster read its own post"
    );
}

#[tokio::test]
async fn members_read_forward_and_the_history_is_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let api = project(&store, "api").await;
    let review = RoomName::parse("review").unwrap();
    let (created, joined) = store
        .writer
        .room_join(api.0, api.1, review.clone(), AgentKind::Codex)
        .await
        .unwrap();
    assert!(created && joined);
    let (created, joined) = store
        .writer
        .room_join(api.0, api.1, review.clone(), AgentKind::Codex)
        .await
        .unwrap();
    assert!(!created && !joined, "joining twice changes nothing");

    for n in 0..(ROOM_HISTORY_CAP + 3) {
        store
            .writer
            .room_post(post(
                api,
                "review",
                AgentKind::ClaudeCode,
                &format!("post {n}"),
            ))
            .await
            .unwrap();
    }
    let (room, posts) = store
        .reader
        .room_posts(api.0, api.1, review.clone(), 0, 1_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(room.last_seq, ROOM_HISTORY_CAP + 3);
    assert_eq!(
        posts.len() as i64,
        ROOM_HISTORY_CAP,
        "older posts were pruned"
    );
    assert_eq!(posts[0].seq, 4);

    assert!(
        store
            .writer
            .room_mark_read(api.0, api.1, review.clone(), AgentKind::Codex, 10)
            .await
            .unwrap()
    );
    store
        .writer
        .room_mark_read(api.0, api.1, review.clone(), AgentKind::Codex, 5)
        .await
        .unwrap();
    let rooms = store.reader.rooms(api.0, api.1).await.unwrap();
    let codex = rooms[0]
        .members
        .iter()
        .find(|m| m.agent == AgentKind::Codex)
        .unwrap();
    assert_eq!(codex.read_seq, 10, "a read position never moves back");
    assert!(
        !store
            .writer
            .room_mark_read(api.0, api.1, review.clone(), AgentKind::Grok, 10)
            .await
            .unwrap(),
        "a non-member has no read position"
    );

    assert!(
        store
            .writer
            .room_leave(api.0, api.1, review.clone(), AgentKind::Codex)
            .await
            .unwrap()
    );
    assert!(
        !store
            .writer
            .room_leave(api.0, api.1, review.clone(), AgentKind::Codex)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn bodies_and_room_counts_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let api = project(&store, "api").await;
    for body in ["", "   ", &"x".repeat(4_001)] {
        assert!(
            store
                .writer
                .room_post(post(api, "general", AgentKind::Codex, body))
                .await
                .is_err(),
            "{} characters",
            body.len()
        );
    }
    for n in 0..MAX_ROOMS_PER_PROJECT {
        store
            .writer
            .room_join(
                api.0,
                api.1,
                RoomName::parse(&format!("r{n}")).unwrap(),
                AgentKind::Codex,
            )
            .await
            .unwrap();
    }
    assert!(
        store
            .writer
            .room_join(
                api.0,
                api.1,
                RoomName::parse("one-more").unwrap(),
                AgentKind::Codex
            )
            .await
            .is_err()
    );
}
