# 产品宣传片 BGM 防收敛指南

当候选音乐过于泛化、过度 ambient，或多个方案听起来几乎相同时，使用本指南。

## 先纠正方向

视觉风格不等于音乐类型。黑色舞台、克制排版和产品聚焦，并不必然要求无鼓、弱低频和持续铺底。多数产品发布或 UI 演示至少需要：

- 清晰但不过度抢戏的节奏底；
- 可记忆的非人声 Hook；
- 能推动镜头的 Bass movement；
- 段落之间的密度与能量差；
- 干净的商业混音和明确收束。

## 防收敛规则

候选不能只换 BPM、形容词或同类音色。每个方向至少在以下六个维度中改变四个：

- Genre：electro-pop product launch、future garage、minimal house、corporate cinematic 等；
- Rhythm：four-on-the-floor、half-time groove、syncopated clicks、breakbeat、无打击乐；
- Bass：warm sub pulse、sidechained synth bass、plucked bass、low strings；
- Hook：synth pluck、piano motif、glass motif、string ostinato、bass riff；
- Energy curve：slow build、early hook、mid-film lift、final drop-out、constant drive；
- Palette：drums + bass + synth、piano + strings、modular synth + percussion 等。

若两个方案在 Genre、Rhythm、Bass、Hook、Palette 中共享三项以上，应重新拉开差异。

## 默认双轨

软件产品宣传片首轮可先比较：

1. **Commercial Product Launch**：强调发布感、CTA 推进和商业完成度；
2. **Clean UI Demo Groove**：强调界面切换、卡片、工作流和精准节奏。

```text
modern commercial product launch music, 118 BPM, instrumental only, polished and confident, tight electronic drums, sidechained synth bass, bright plucked synth hook, warm pads, clean impact hits. Structure: sparse branded intro for 0-6s, groove enters at 6s, short impact at 14s, full product-demo beat from 15-34s, stronger lift from 34-54s, clean final resolve at 54-60s. No vocals, no lyrics, no ambient-only bed, no sleepy pads, no trap, no cinematic trailer booms.
```

```text
clean UI demo groove, 112 BPM, instrumental only, precise and upbeat but not loud, muted kick, crisp snare snaps, syncopated digital percussion, rubbery synth bass, short glass-pluck motif, light airy pads. Structure: minimal boot-up intro, groove starts at 6s, UI reveal hit at 15s, card grid rhythm from 24s, tighter workflow pulse from 34s, wider final lift from 44s, quick clean ending. No vocals, no lyrics, no slow ambient wash, no generic corporate piano, no EDM festival drop.
```

## 其他候选

### 有情绪推进的创始人叙事

```text
emotional founder-product film score with modern electronic groove, 98 BPM, instrumental only, focused and hopeful, soft piano motif, warm synth bass, brushed electronic drums, subtle strings, glass accents. Structure: lonely sparse intro, tension at 6s, product reveal opens harmony at 15s, rhythm organizes at 24s, stronger movement at 34s, warm confident lift at 44s, resolved phrase at 54s. No vocals, no lyrics, no sad piano ballad, no ambient-only pad, no heavy trailer drums.
```

### 克制的高管级电影感

```text
executive cinematic product score with restrained beat, 102 BPM, instrumental only, serious and forward-moving, low strings, soft piano pulses, tight hybrid percussion, controlled synth bass, clean metallic hits. Structure: serious opening, tension at 6s, brand hit at 14s, UI reveal at 15s, measured card rhythm at 24s, stronger process momentum at 34s, broader lift at 44s, resolved cadence at 54s. No vocals, no lyrics, no cheesy corporate piano, no ambient-only bed, no trailer booms, no trap hats.
```

## Mureka 适配

- 生成 prompt 不超过 1024 字符。
- 首句先写 Genre、BPM、Rhythm、Bass 和 Hook。
- 明确写入 `instrumental only` 与 `no vocals, no lyrics`。
- 不要让每个候选都重复 `minimal`、`ambient`、`pads`、`glass`、`ticks`。
- 不默认禁用所有鼓，只排除不合适的鼓组，如 `no trap hats`、`no EDM festival drop`。
- 音乐身份明确后再加入时间结构与卡点；实际时间码应按当前 EDL 改写。
