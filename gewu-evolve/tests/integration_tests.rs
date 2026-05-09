use gewu_evolve::SkillLibrary;
use tempfile::TempDir;

#[tokio::test]
async fn test_skill_library_save_and_load() {
    let temp_dir = TempDir::new().unwrap();
    let skill_dir = temp_dir.path().to_path_buf();

    let library = SkillLibrary::new(skill_dir).unwrap();

    let skill = gewu_evolve::Skill {
        name: "test_skill".to_string(),
        version: "1.0".to_string(),
        description: "A test skill".to_string(),
        parameters: serde_json::json!({}),
        implementation: gewu_evolve::SkillImplementation {
            language: "python".to_string(),
            code: "def main(args): return 'hello'".to_string(),
            dependencies: vec![],
        },
        created_at: chrono::Utc::now(),
    };

    library.save_skill(&skill).await.unwrap();

    let loaded = library.load_skill("test_skill").await.unwrap();
    assert!(loaded.is_some());
}

#[tokio::test]
async fn test_skill_library_list_skills() {
    let temp_dir = TempDir::new().unwrap();
    let skill_dir = temp_dir.path().to_path_buf();

    let library = SkillLibrary::new(skill_dir).unwrap();

    let skill = gewu_evolve::Skill {
        name: "list_test_skill".to_string(),
        version: "1.0".to_string(),
        description: "A skill for listing test".to_string(),
        parameters: serde_json::json!({}),
        implementation: gewu_evolve::SkillImplementation {
            language: "python".to_string(),
            code: "def main(args): pass".to_string(),
            dependencies: vec![],
        },
        created_at: chrono::Utc::now(),
    };

    library.save_skill(&skill).await.unwrap();

    let list = library.list_skills().unwrap();
    assert!(!list.is_empty());
    assert_eq!(list[0].name, "list_test_skill");
}

#[tokio::test]
async fn test_skill_library_delete() {
    let temp_dir = TempDir::new().unwrap();
    let skill_dir = temp_dir.path().to_path_buf();

    let library = SkillLibrary::new(skill_dir).unwrap();

    let skill = gewu_evolve::Skill {
        name: "delete_me".to_string(),
        version: "1.0".to_string(),
        description: "A skill to delete".to_string(),
        parameters: serde_json::json!({}),
        implementation: gewu_evolve::SkillImplementation {
            language: "python".to_string(),
            code: "def main(args): pass".to_string(),
            dependencies: vec![],
        },
        created_at: chrono::Utc::now(),
    };

    library.save_skill(&skill).await.unwrap();
    library.delete_skill("delete_me").await.unwrap();

    let loaded = library.load_skill("delete_me").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn test_skill_library_load_nonexistent() {
    let temp_dir = TempDir::new().unwrap();
    let skill_dir = temp_dir.path().to_path_buf();

    let library = SkillLibrary::new(skill_dir).unwrap();
    let result = library.load_skill("does_not_exist").await.unwrap();
    assert!(result.is_none());
}