# Mureka 音乐 Prompt 指南

## 基本原则

用英文描述音乐，不向模型下命令：

```text
不推荐：Create an energetic pop song with drums
推荐：energetic synth-pop, 128 BPM, driving four-on-the-floor drums, bright synth hooks, instrumental only
```

Prompt 先确定声音身份，再描述动态弧线。只写一个静态情绪，容易得到从头到尾密度相同的结果。

```text
sparse branded opening → rising rhythmic tension → full product reveal → wider final lift → clean resolve
```

## 推荐结构

```text
[specific genre and era], [mood], [BPM], [instrumental or vocal],
[rhythm], [bass], [hook], [3-5 instruments],
[dynamic arc or time-coded structure],
[mixing notes], [avoid list]
```

参数必须彼此一致。例如 `slow and relaxing` 与 `160 BPM, high energy` 会产生冲突。避免只写 `nice pop`、`premium` 或 `tech background` 等宽泛标签。

不要要求模仿在世艺人或仅写 “sound like [artist]”。应拆解为可描述的制作特征，例如 `warm analog synths, driving bass, 1980s production texture`。

## Mureka 限制与工具行为

- 生成 prompt 最多 1024 字符。
- 歌词最多 3000 字符。
- `n` 最多为 3；脚本不传 `-n` 时由 API 使用其默认行为。
- 支持下载 `mp3`、`flac` 或 `wav`。
- 默认模型参数为 `mureka-8`。
- 默认 API 地址为 `https://api.mureka.ai`，可通过 `MUREKA_API_URL` 覆盖。
- 所有 API 调用需要 `MUREKA_API_KEY`；生成任务由脚本轮询至终态，默认间隔 5 秒、超时 600 秒。

## 文件上传用途

`python scripts/mureka.py upload <file> --purpose <purpose>` 调用 `/v1/files/upload`。当前有效 purpose 与官方限制如下：

- `reference`：`mp3`、`m4a`；音频时长为 30 秒，超出部分会被裁剪。
- `melody`：`mp3`、`m4a`、`mid`；音频时长 5—60 秒，超出部分会被裁剪。
- `instrumental`：`mp3`、`m4a`；音频时长为 30 秒，超出部分会被裁剪。
- `voice`：`mp3`、`m4a`；音频时长 5—15 秒，超出部分会被裁剪。
- `audio`：`mp3`、`m4a`；文件不超过 10 MB，官方端点未声明时长范围。
- `remix`：`mp3`、`m4a`；音频时长 10—350 秒。
- `soundtrack`：视频支持 `mp4`、`mov`、`avi`、`mkv`、`webm`，不超过 100 MB；图片支持 `jpg`、`jpeg`、`png`、`webp`，不超过 50 MB；官方端点未声明时长范围。
- `lyrics-video`：图片支持 `jpg`、`jpeg`、`png`、`webp`、`gif`；官方端点未在该项声明时长或大小限制。

`vocal` 已不是上传 purpose。歌曲命令的 `--vocal-id` 仍用于传入已有 Vocal ID，二者不要混淆。

## 宣传片纯音乐

产品宣传片 prompt 应明确：

- Genre 与商业语气；
- BPM、Rhythm 和 Bass movement；
- 非人声 Hook；
- 画面卡点对应的 impact、drop、lift 或 resolve；
- `instrumental only` 和 `no vocals, no lyrics`；
- 不希望出现的具体元素，而不是笼统禁止所有鼓或所有合成器。

时间结构必须依据当前 EDL 调整，不能机械复用示例时间码。

## 歌词任务

需要歌词时使用 `[Intro]`、`[Verse]`、`[Pre-Chorus]`、`[Chorus]`、`[Bridge]`、`[Break]`、`[Outro]` 等结构标签。每行尽量控制在 6—10 个英文单词，相邻行保持接近的音节数；副歌应比主歌更短、更重复。

歌词生成与扩写命令：

```bash
python scripts/mureka.py lyrics generate "a bittersweet farewell song"
python scripts/mureka.py lyrics extend "[Verse]\nExisting lyrics..."
```

## 多候选与迭代

首轮生成 2—3 个候选，不依赖单一结果。宣传片方向使用 anti-convergence：候选至少在 Genre、Rhythm、Bass、Hook、Energy curve、Palette 六个维度中改变四个。

试听每个候选并记录：

- 节奏是否支持剪辑；
- Hook 是否可辨认但不抢旁白；
- 乐器是否实际出现；
- 能量变化是否命中镜头；
- 结尾是否便于剪切和收束；
- 是否出现禁用元素。

每轮只改一个变量，例如 BPM、鼓组、Hook、音色或能量曲线；保留其他条件后重新生成，才能判断变化是否有效。

## 生成前检查

- [ ] Genre 足够具体，情绪描述不冲突；
- [ ] BPM、Rhythm、Bass、Hook 和 3—5 个主要音色齐全；
- [ ] 已写明 `instrumental only` 或具体人声类型；
- [ ] 动态弧线与关键卡点匹配 EDL；
- [ ] Avoid list 具体且不过度；
- [ ] Prompt 未超过 1024 字符；
- [ ] 计划生成多个候选；
- [ ] 已确认 key、依赖、额度与输出目录；
- [ ] 未把任务提交或 prompt 交付误报为音频生成成功。
