use aionui_common::AgentType;
use aionui_db::init_database_memory;

#[tokio::test]
async fn migrates_the_internal_agent_to_the_branded_workspace_skills_path() {
    let db = init_database_memory().await.expect("in-memory database");
    let raw: String = sqlx::query_scalar(
        "SELECT native_skills_dirs FROM agent_metadata \
         WHERE agent_id = '632f31d2' AND agent_type = 'aionrs' AND agent_source = 'internal'",
    )
    .fetch_one(db.pool())
    .await
    .expect("internal WorkMate agent row");

    let migrated_dirs: Vec<String> = serde_json::from_str(&raw).expect("valid skills directory list");
    let compiled_dirs: Vec<String> = AgentType::Aionrs
        .native_skills_dirs()
        .expect("WorkMate discovers skills natively")
        .iter()
        .map(|path| (*path).to_owned())
        .collect();

    assert_eq!(migrated_dirs, vec![".csbu-workmate/skills"]);
    assert_eq!(migrated_dirs, compiled_dirs);
}
