use super::should_hide;

#[test]
fn hides_vcs_internals() {
    assert!(should_hide(".git"));
}

#[test]
fn hides_macos_junk_case_insensitively() {
    assert!(should_hide(".DS_Store"));
    assert!(should_hide(".ds_store"));
    assert!(should_hide(".Spotlight-V100"));
    assert!(should_hide(".Trashes"));
    assert!(should_hide(".fseventsd"));
    // AppleDouble sidecars match by prefix.
    assert!(should_hide("._resource"));
    assert!(should_hide("._DS_Store"));
}

#[test]
fn hides_windows_junk_case_insensitively() {
    assert!(should_hide("Thumbs.db"));
    assert!(should_hide("thumbs.db"));
    assert!(should_hide("desktop.ini"));
    assert!(should_hide("Desktop.ini"));
    assert!(should_hide("$RECYCLE.BIN"));
}

#[test]
fn hides_linux_trash_by_prefix() {
    assert!(should_hide(".Trash-1000"));
    assert!(should_hide(".directory"));
}

#[test]
fn keeps_real_dotfiles_users_want() {
    // Neither exact nor prefix noise — must stay visible.
    assert!(!should_hide(".env"));
    assert!(!should_hide(".github"));
    assert!(!should_hide(".gitignore"));
    assert!(!should_hide(".gitmodules"));
    assert!(!should_hide("git")); // no leading dot
    assert!(!should_hide("README.md"));
    assert!(!should_hide("src"));
    assert!(!should_hide("Button.tsx"));
}
