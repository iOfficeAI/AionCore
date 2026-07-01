use aionui_conversation::response_middleware::strip_think_tags;

#[test]
fn strip_think_tags_removes_private_reasoning_blocks() {
    let input = "前文<think>内部推理</think>后文";
    assert_eq!(strip_think_tags(input), "前文后文");
}

#[test]
fn strip_thinking_tags_removes_multiline_blocks() {
    let input = "开始\n<thinking>\n第一行\n第二行\n</thinking>\n结束";
    assert_eq!(strip_think_tags(input), "开始\n\n结束");
}
