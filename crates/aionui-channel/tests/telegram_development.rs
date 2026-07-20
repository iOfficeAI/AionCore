use aionui_channel::development::DevelopmentHandoffSigner;

#[test]
fn handoff_links_are_scoped_signed_and_expiring() {
    let signer = DevelopmentHandoffSigner::new([7; 32], "/#/projects");
    let expires_at = 10_000;
    let link = signer.sign("project one", "run/one", expires_at);

    assert!(link.contains("projectId=project%20one"));
    assert!(link.contains("runId=run%2Fone"));
    let signature = link.split("signature=").nth(1).unwrap();
    assert!(signer.verify("project one", "run/one", expires_at, signature, 9_000));
    assert!(!signer.verify("project two", "run/one", expires_at, signature, 9_000));
    assert!(!signer.verify("project one", "run/one", expires_at, signature, 10_001));
}

#[test]
fn handoff_links_reject_unreasonably_long_lifetimes() {
    let signer = DevelopmentHandoffSigner::new([3; 32], "/#/projects");
    let expires_at = 24 * 60 * 60 * 1000 + 1;
    let link = signer.sign("project", "run", expires_at);
    let signature = link.split("signature=").nth(1).unwrap();
    assert!(!signer.verify("project", "run", expires_at, signature, 0));
}
