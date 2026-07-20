# Image input model allowlist

`image_input_models.json` is a hand-maintained AionCore asset. It is embedded at compile time and is never downloaded or refreshed at runtime. Its API roots mirror the fixed `base_url` presets in AionUi's `modelPlatforms.ts`; model IDs are maintained independently from `models.dev`.

The catalog is intentionally a positive allowlist:

- Match both the provider API root and the exact model ID.
- Add a model only when the provider's own documentation confirms image input on the API protocol used by AionCore.
- Treat an absent provider or model as `Unknown`, not as proof that image input is unsupported.
- Do not copy a first-party model entry to an aggregator or a custom gateway. Those endpoints may expose different model IDs or capabilities.
- Keep an empty `models` array when the preset endpoint is known but no stable model ID can be positively verified for that endpoint. This still records the AionUi preset without claiming image support.
- Aggregator entries may be refreshed manually from that aggregator's own catalog. The reviewed result must be committed as a static snapshot; AionCore never fetches it at runtime.

Poe bot names and Ctyun deployment model IDs are account- or deployment-specific, so they intentionally have no static model entries. DeepSeek does not currently expose a positively verified image-input chat model on the corresponding AionUi preset endpoint.

The list was last reviewed on 2026-07-15 against these provider-owned references:

- OpenAI: https://developers.openai.com/api/docs/models
- Anthropic and Bedrock model IDs: https://platform.claude.com/docs/en/about-claude/models/overview
- Amazon Bedrock image messages: https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-anthropic-claude-messages.html
- Gemini: https://ai.google.dev/gemini-api/docs/models
- AionUi provider presets: https://github.com/iOfficeAI/AionUi/blob/main/packages/desktop/src/renderer/utils/model/modelPlatforms.ts
- Novita model library and vision guide: https://novita.ai/models and https://novita.ai/docs/guides/llm-vision
- OpenRouter model catalog and image inputs: https://openrouter.ai/api/v1/models and https://openrouter.ai/docs/guides/overview/multimodal/image-understanding
- MiniMax OpenAI-compatible Chat Completions schema: https://platform.minimaxi.com/docs/api-reference/text/api/openapi-chat-openai.json
- Dashscope: https://help.aliyun.com/en/model-studio/vision-model/
- SiliconFlow model library and multimodal inputs: https://www.siliconflow.com/models/vision and https://docs.siliconflow.cn/cn/userguide/capabilities/multimodal-vision
- Zhipu: https://docs.bigmodel.cn/cn/guide/models/vlm/glm-5v-turbo
- Moonshot: https://platform.kimi.ai/docs/models
- xAI: https://docs.x.ai/developers/model-capabilities/images/understanding
- Volcengine Ark: https://www.volcengine.com/docs/82379/1795150
- Qianfan: https://cloud.baidu.com/doc/qianfan-docs/s/fm8r1ndsm
- Tencent Hunyuan: https://cloud.tencent.com/document/product/1729/104753 and https://cloud.tencent.com/document/product/1729/111007
- Lingyi: https://platform.lingyiwanwu.com/
- PPIO: https://ppio.com/docs/model/visual and https://ppio.com/pricing
- ModelScope API-Inference: https://www.modelscope.cn/docs/model-service/API-Inference/intro
- InfiniAI: https://docs.infini-ai.com/gen-studio/api/multimodal/tutorial-vision.html
- Ctyun endpoint behavior: https://www.ctyun.cn/document/10541165/10876778
- StepFun: https://platform.stepfun.com/docs/zh/guides/models/vision
