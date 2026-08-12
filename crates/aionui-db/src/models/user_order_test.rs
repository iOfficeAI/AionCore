use super::{OrderItemType, OrderScene};

#[test]
fn order_scene_roundtrips_through_column_value() {
    for scene in [OrderScene::Pinned] {
        assert_eq!(OrderScene::parse(scene.as_str()), Some(scene));
    }
    assert_eq!(OrderScene::parse("unknown"), None);
}

#[test]
fn order_item_type_roundtrips_through_column_value() {
    for item_type in [OrderItemType::Conversation, OrderItemType::Team] {
        assert_eq!(OrderItemType::parse(item_type.as_str()), Some(item_type));
    }
    assert_eq!(OrderItemType::parse("unknown"), None);
}
