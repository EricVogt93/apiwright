use forge_core::model::{FolderMeta, Method, RequestDef};
use forge_core::store::{
    create_collection, create_folder, create_request, load_json, rename_folder, set_order,
    Workspace, COLLECTION_FILE, FOLDER_FILE,
};

#[test]
fn rejected_folder_rename_preserves_metadata_and_children() {
    let root = tempfile::tempdir().unwrap();
    let original = create_folder(root.path(), "Original").unwrap();
    let occupied = create_folder(root.path(), "Occupied").unwrap();
    let request = create_request(
        &original,
        &RequestDef::new("One", Method::Get, "https://example.test"),
    )
    .unwrap();
    let old_meta = std::fs::read(original.join(FOLDER_FILE)).unwrap();
    let old_request = std::fs::read(&request).unwrap();
    let occupied_meta = std::fs::read(occupied.join(FOLDER_FILE)).unwrap();
    assert!(rename_folder(&original, "Occupied").is_err());
    assert_eq!(std::fs::read(original.join(FOLDER_FILE)).unwrap(), old_meta);
    assert_eq!(std::fs::read(request).unwrap(), old_request);
    assert_eq!(
        std::fs::read(occupied.join(FOLDER_FILE)).unwrap(),
        occupied_meta
    );
}

#[test]
fn invalid_parent_order_does_not_partially_rename_folder() {
    let root = tempfile::tempdir().unwrap();
    Workspace::create(root.path(), "Test").unwrap();
    let collection = create_collection(root.path(), "Collection").unwrap();
    let old = create_folder(&collection, "Original").unwrap();
    let before = std::fs::read(old.join(FOLDER_FILE)).unwrap();
    std::fs::write(collection.join(COLLECTION_FILE), b"invalid metadata").unwrap();
    assert!(rename_folder(&old, "New").is_err());
    assert!(old.is_dir());
    assert!(!collection.join("new").exists());
    assert_eq!(std::fs::read(old.join(FOLDER_FILE)).unwrap(), before);
    assert_eq!(
        std::fs::read(collection.join(COLLECTION_FILE)).unwrap(),
        b"invalid metadata"
    );
}

#[test]
fn failed_metadata_replacement_rolls_back_directory_move() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old");
    std::fs::create_dir_all(old.join(FOLDER_FILE)).unwrap();
    std::fs::write(old.join("keep.txt"), "keep me").unwrap();
    assert!(rename_folder(&old, "New").is_err());
    assert!(old.is_dir());
    assert!(!root.path().join("new").exists());
    assert_eq!(
        std::fs::read_to_string(old.join("keep.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn successful_folder_rename_updates_name_order_and_preserves_request() {
    let root = tempfile::tempdir().unwrap();
    let parent = create_folder(root.path(), "Parent").unwrap();
    let old = create_folder(&parent, "Old").unwrap();
    let request = create_request(
        &old,
        &RequestDef::new("One", Method::Get, "https://example.test"),
    )
    .unwrap();
    let bytes = std::fs::read(&request).unwrap();
    set_order(&parent, vec!["old".into(), "sibling".into()]).unwrap();
    let new = rename_folder(&old, "New").unwrap();
    assert!(!old.exists());
    assert_eq!(
        load_json::<FolderMeta>(&new.join(FOLDER_FILE))
            .unwrap()
            .name,
        "New"
    );
    assert_eq!(
        load_json::<FolderMeta>(&parent.join(FOLDER_FILE))
            .unwrap()
            .order,
        ["new", "sibling"]
    );
    assert_eq!(
        std::fs::read(new.join(request.file_name().unwrap())).unwrap(),
        bytes
    );
}
