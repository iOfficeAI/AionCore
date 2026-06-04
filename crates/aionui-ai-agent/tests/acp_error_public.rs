use aionui_ai_agent::AcpError;

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn acp_error_is_public_error_contract() {
    assert_error::<AcpError>();
}
