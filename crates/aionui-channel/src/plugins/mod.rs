#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(feature = "lark")]
pub mod lark;

#[cfg(feature = "dingtalk")]
pub mod dingtalk;

#[cfg(feature = "weixin")]
pub mod weixin;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "discord")]
pub mod discord;

use crate::plugin::ChannelPlugin;
use crate::types::PluginType;

/// Create a platform-specific plugin instance from a `PluginType`.
///
/// Returns `None` if the platform feature is not compiled in.
pub fn create_plugin(plugin_type: PluginType) -> Option<Box<dyn ChannelPlugin>> {
    match plugin_type {
        #[cfg(feature = "telegram")]
        PluginType::Telegram => Some(Box::new(telegram::TelegramPlugin::new())),

        #[cfg(feature = "lark")]
        PluginType::Lark => Some(Box::new(lark::LarkPlugin::new())),

        #[cfg(feature = "dingtalk")]
        PluginType::Dingtalk => Some(Box::new(dingtalk::DingtalkPlugin::new())),

        #[cfg(feature = "weixin")]
        PluginType::Weixin => Some(Box::new(weixin::WeixinPlugin::new())),

        #[cfg(feature = "slack")]
        PluginType::Slack => Some(Box::new(slack::SlackPlugin::new())),

        #[cfg(feature = "discord")]
        PluginType::Discord => Some(Box::new(discord::DiscordPlugin::new())),

        #[allow(unreachable_patterns)]
        _ => None,
    }
}
