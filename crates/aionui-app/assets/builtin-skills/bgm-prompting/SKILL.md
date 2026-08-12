---
name: bgm-prompting
description: |
  产品宣传片 BGM 提示词设计与 Mureka 生成工具。
  适用于配乐方向、英文音乐 prompt、卡点、候选防收敛和 Mureka 音频生成。
---

# BGM 提示词设计

用于把宣传片简报、分镜和 EDL 转化为可比较、可剪辑的音乐方向。业务说明使用简体中文；提交给音乐模型的 prompt 保持英文。

## 使用顺序

1. 从视频时长、镜头和转场提取情绪曲线与卡点。
2. 先定 Genre、BPM、Rhythm、Bass、Hook、Palette，再写时间结构和避免项。
3. 默认设计 2—3 个候选；候选至少在六个维度中改变四个。
4. 先交付英文 prompts 和卡点表，用户确认并允许消耗额度后再生成。
5. 生成多个结果，逐一试听；每轮只修改一个变量。

## 核心原则

- Prompt 描述音乐本身，不使用 “create” 或 “make” 等命令句。
- 至少包含具体风格、BPM、情绪、3—5 个音色、动态弧线和 `instrumental only`。
- 产品宣传片应有节奏、记忆点、低频运动和段落变化，避免所有候选收敛为 ambient 垫底。
- Mureka 生成 prompt 不超过 1024 字符；歌词不超过 3000 字符。
- 未配置 `MUREKA_API_KEY`、未安装依赖或 API 调用失败时，不得声称音频已生成。

## 参考资料

- `references/mureka_prompt_guide.md`：提示词结构、动态弧线、限制和迭代方法。
- `references/product_promo_bgm_prompting.md`：宣传片防收敛、默认双轨和候选方向。

## 生成命令

在本 skill 目录安装依赖并执行：

```bash
pip install -r requirements.txt
python scripts/mureka.py instrumental \
  --prompt "<validated English prompt>" \
  -n 3 --format mp3 \
  --output assets/bgm/
```

必须设置 `MUREKA_API_KEY`；可用 `MUREKA_API_URL` 覆盖默认地址 `https://api.mureka.ai`。只有任务成功、音频文件实际存在并完成检查后，才能报告生成成功。
