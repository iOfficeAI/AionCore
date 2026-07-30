use std::sync::Arc;

use aionui_db::{Database, IProjectStore, ProjectKind, SqliteProjectStore, init_database_memory};

async fn store() -> (Arc<dyn IProjectStore>, Database) {
    let db = init_database_memory().await.unwrap();
    let s = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    (s as Arc<dyn IProjectStore>, db)
}

#[tokio::test]
async fn upsert_folder_is_idempotent_by_canonical() {
    let (store, _db) = store().await;

    let a = store
        .upsert_folder("file:///Users/me/proj", "file:///Users/me/proj")
        .await
        .unwrap();
    // Same canonical, different raw uri: must hit the existing row and
    // preserve the first resource_uri (folder rows are immutable once created).
    let b = store
        .upsert_folder("file:///Users/me/proj", "file:///Users/me/proj-alias")
        .await
        .unwrap();

    assert_eq!(a.folder_id, b.folder_id);
    assert_eq!(a.resource_uri, "file:///Users/me/proj");
    assert_eq!(b.resource_uri, "file:///Users/me/proj");
    assert_eq!(b.resource_canonical, "file:///Users/me/proj");
}

#[tokio::test]
async fn distinct_canonical_yields_distinct_folder() {
    let (store, _db) = store().await;
    let a = store.upsert_folder("file:///a", "file:///a").await.unwrap();
    let b = store.upsert_folder("file:///b", "file:///b").await.unwrap();
    assert_ne!(a.folder_id, b.folder_id);
}

#[tokio::test]
async fn create_project_with_workspace_entry_builds_consistent_pair() {
    let (store, _db) = store().await;
    let folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();

    let (project, entry) = store
        .create_project_with_workspace_entry("system_default_user", &folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();

    assert_eq!(project.name, "ws");
    assert_eq!(project.kind, "standard");
    assert_eq!(entry.project_id, project.project_id);
    assert_eq!(entry.folder_id, folder.folder_id);
    assert_eq!(entry.role, "workspace");

    // Readable back.
    assert_eq!(
        store
            .get_project("system_default_user", &project.project_id)
            .await
            .unwrap()
            .unwrap()
            .project_id,
        project.project_id
    );
    assert_eq!(
        store
            .get_entry("system_default_user", &entry.pe_id)
            .await
            .unwrap()
            .unwrap()
            .pe_id,
        entry.pe_id
    );
    let ws = store
        .select_workspace_entry_by_folder("system_default_user", &folder.folder_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ws.pe_id, entry.pe_id);
}

#[tokio::test]
async fn create_project_with_workspace_entry_falls_back_on_workspace_conflict() {
    let (store, _db) = store().await;
    let folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();

    let (p1, e1) = store
        .create_project_with_workspace_entry("system_default_user", &folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();
    // Second create for the SAME workspace folder must not create a rival
    // workspace project; it returns the existing pair (idempotent race guard).
    let (p2, e2) = store
        .create_project_with_workspace_entry("system_default_user", &folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();

    assert_eq!(p1.project_id, p2.project_id);
    assert_eq!(e1.pe_id, e2.pe_id);
}

#[tokio::test]
async fn list_entries_returns_workspace_and_attached_with_folders() {
    let (store, _db) = store().await;
    let ws_folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();
    let (project, _ws_entry) = store
        .create_project_with_workspace_entry("system_default_user", &ws_folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();

    let att_folder = store.upsert_folder("file:///att", "file:///att").await.unwrap();
    let att = store
        .insert_attached_entry(
            "system_default_user",
            &project.project_id,
            &att_folder.folder_id,
            Some("Docs"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(att.role, "attached");
    assert_eq!(att.display_name.as_deref(), Some("Docs"));

    let entries = store
        .list_entries("system_default_user", &project.project_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    // Every entry carries its joined folder.
    for (entry, folder) in &entries {
        assert_eq!(entry.folder_id, folder.folder_id);
    }
}

#[tokio::test]
async fn remove_entry_deletes_only_the_relation() {
    let (store, _db) = store().await;
    let ws_folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();
    let (project, _) = store
        .create_project_with_workspace_entry("system_default_user", &ws_folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();
    let att_folder = store.upsert_folder("file:///att", "file:///att").await.unwrap();
    let att = store
        .insert_attached_entry(
            "system_default_user",
            &project.project_id,
            &att_folder.folder_id,
            None,
            10,
        )
        .await
        .unwrap();

    store.remove_entry("system_default_user", &att.pe_id).await.unwrap();

    assert!(
        store
            .get_entry("system_default_user", &att.pe_id)
            .await
            .unwrap()
            .is_none()
    );
    // Folder row survives (not deleted with the relation).
    assert!(store.get_folder(&att_folder.folder_id).await.unwrap().is_some());
    // Workspace entry untouched.
    assert_eq!(
        store
            .list_entries("system_default_user", &project.project_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn rename_entry_updates_display_name() {
    let (store, _db) = store().await;
    let ws_folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();
    let (project, _) = store
        .create_project_with_workspace_entry("system_default_user", &ws_folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();
    let att_folder = store.upsert_folder("file:///att", "file:///att").await.unwrap();
    let att = store
        .insert_attached_entry(
            "system_default_user",
            &project.project_id,
            &att_folder.folder_id,
            Some("Old"),
            10,
        )
        .await
        .unwrap();

    let renamed = store
        .rename_entry("system_default_user", &att.pe_id, Some("New"))
        .await
        .unwrap();
    assert_eq!(renamed.display_name.as_deref(), Some("New"));

    let cleared = store
        .rename_entry("system_default_user", &att.pe_id, None)
        .await
        .unwrap();
    assert_eq!(cleared.display_name, None);
}

#[tokio::test]
async fn reorder_sets_order_index_by_position() {
    let (store, _db) = store().await;
    let ws_folder = store.upsert_folder("file:///ws", "file:///ws").await.unwrap();
    let (project, ws_entry) = store
        .create_project_with_workspace_entry("system_default_user", &ws_folder.folder_id, "ws", ProjectKind::Standard)
        .await
        .unwrap();
    let f1 = store.upsert_folder("file:///a1", "file:///a1").await.unwrap();
    let f2 = store.upsert_folder("file:///a2", "file:///a2").await.unwrap();
    let a1 = store
        .insert_attached_entry("system_default_user", &project.project_id, &f1.folder_id, None, 10)
        .await
        .unwrap();
    let a2 = store
        .insert_attached_entry("system_default_user", &project.project_id, &f2.folder_id, None, 20)
        .await
        .unwrap();

    let ordered = vec![a2.pe_id.clone(), ws_entry.pe_id.clone(), a1.pe_id.clone()];
    store
        .reorder("system_default_user", &project.project_id, &ordered)
        .await
        .unwrap();

    let mut by_pe: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (entry, _) in store
        .list_entries("system_default_user", &project.project_id)
        .await
        .unwrap()
    {
        by_pe.insert(entry.pe_id, entry.order_index);
    }
    assert_eq!(by_pe[&a2.pe_id], 0);
    assert_eq!(by_pe[&ws_entry.pe_id], 1);
    assert_eq!(by_pe[&a1.pe_id], 2);
}

#[tokio::test]
async fn get_missing_project_and_entry_return_none() {
    let (store, _db) = store().await;
    assert!(
        store
            .get_project("system_default_user", "nope")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get_entry("system_default_user", "nope").await.unwrap().is_none());
    assert!(store.get_folder("nope").await.unwrap().is_none());
    assert!(
        store
            .select_workspace_entry_by_folder("system_default_user", "nope")
            .await
            .unwrap()
            .is_none()
    );
}
