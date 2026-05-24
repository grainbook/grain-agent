//! End-to-end check: harness pieces compose with grain-agent-core.

use grain_agent_core::{AgentMessage, Message, TextContent, UserContent, UserMessage};
use grain_agent_harness::{
    InMemorySessionRepo, SessionRepo, branch_summary_message, compaction_summary_message,
    convert_to_llm, custom_message, format_skills_for_system_prompt, system_prompt::Skill,
};

#[tokio::test]
async fn convert_to_llm_handles_custom_messages() {
    let messages = vec![
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: "hi".into() })],
            timestamp: 1,
        }),
        compaction_summary_message("prior conversation summary", 12_345, 2),
        branch_summary_message("branch summary text", "entry-x", 3),
        custom_message(
            "artifact",
            serde_json::json!("inline string content"),
            true,
            None,
            4,
        ),
    ];

    let out = convert_to_llm(messages);
    assert_eq!(out.len(), 4);
    for m in &out {
        assert!(matches!(m, Message::User(_)));
    }
    if let Message::User(u) = &out[1] {
        let text = match &u.content[0] {
            UserContent::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("<summary>"));
        assert!(text.contains("prior conversation summary"));
    }
}

#[tokio::test]
async fn skills_render_into_system_prompt_xml() {
    let skills = vec![
        Skill {
            name: "Bash".into(),
            description: "Runs shell commands".into(),
            file_path: "/skills/bash/SKILL.md".into(),
            disable_model_invocation: false,
            body: String::new(),
        },
        Skill {
            name: "Hidden".into(),
            description: "Hidden skill".into(),
            file_path: "/skills/hidden/SKILL.md".into(),
            disable_model_invocation: true,
            body: String::new(),
        },
    ];
    let rendered = format_skills_for_system_prompt(&skills);
    assert!(rendered.contains("<name>Bash</name>"));
    assert!(rendered.contains("<location>/skills/bash/SKILL.md</location>"));
    assert!(!rendered.contains("Hidden"));
}

#[tokio::test]
async fn session_repo_round_trip() {
    let repo = InMemorySessionRepo::new();
    let session = repo.create(None).await.unwrap();
    let id1 = session
        .append_message(AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: "one".into() })],
            timestamp: 0,
        }))
        .await
        .unwrap();
    let id2 = session
        .append_message(AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: "two".into() })],
            timestamp: 0,
        }))
        .await
        .unwrap();
    assert_ne!(id1, id2);
    let branch = session.branch(None).await;
    assert_eq!(branch.len(), 2);
    assert_eq!(branch[0].id, id1);
    assert_eq!(branch[1].id, id2);
    assert_eq!(branch[1].parent_id.as_deref(), Some(id1.as_str()));
}
