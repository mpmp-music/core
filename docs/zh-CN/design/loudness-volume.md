[English](../../design/loudness-volume.md) · 本文为英文版的译文，如有出入以英文版为准。

# 响度与音量

相关实现为 core 的 `gain.rs` / `loudness_scan.rs`、engine 的增益与分析路径，以及 library 的响度缓存。现行接口见 [Schema](../../contract.schema.json)。

默认后台自动补齐缺失/失效的响度数据，播放不等待全库测量；不提供日常扫描、清空和重算按钮。有效文件标签可先供播放，有效派生缓存优先。结果只写数据库，不修改音乐文件标签。文件、CUE、范围或版本变化使缓存失效；文件大小/mtime 模型不能识别刻意保持两者的内容替换。

自动遍历按 ID 分批读取；波形与响度共享单分析解码闸门，actor 只调度和发布事件。曲目结果分批提交，专辑必须等所有当前可用可见成员具有有效统计后，从持久化门限统计聚合，不平均 LUFS。静音/有效块不足记录为已测量但无综合值，不制造有限 LUFS。永久解码错误按版本记忆，暂时错误有退避；版本变化才重新分析。后台事件 origin=null；自动 LoudnessScan 的 total=null，done 包含缓存命中，不是新增结果数。JobId 不跨 core 运行有效。

## 增益

软件音量默认启用，滑块 volume∈[0,1]，实际倍数为 volume³；静音单独控制。系统音量模式目前返回 unsupported/not_implemented。ReplayGain 有关闭、单曲、专辑、智能四种，默认智能；智能在非随机且专辑上下文或相邻同专辑时用专辑增益，其余用单曲，缺失时回退另一种。

```text
有数据：dB = 所选增益 + preamp_db
无数据：dB = fallback_preamp_db
防削波：有峰值时限制 dB ≤ -20 log10(peak)
最终倍数 = 10^(dB/20) × volume³
```

SetReplayGain 是局部更新，省略项保持原值。无 RG 数据、有效零增益与关闭是不同状态；信号路径要分别展示，不能因最终倍数为 1 就隐藏数据来源。音量/增益过渡约 10 ms 平滑，已缓冲音频可能保留旧增益。相邻曲目降低增益在前一首末尾过渡，提高增益在后一首开头，避免放大后一首的起音。

单位倍数可跳过乘法，但 bit_perfect 是完整路径的唯一结论，不能只从音量推断。重采样、混音、量化、dither、后端和未验证设备路径都会影响结论。16 位输出按实际处理/源精度决定 TPDF dither；I24 为三字节，24-in-32 用 I32 和 valid_bits_per_sample=24。verified/exclusive_verified 只覆盖端点驱动边界，不能证明 DAC 内部逐位一致。当前 CPAL 路径不因契约有 exclusive 枚举就获得独占能力；运行时设备故障保留用户偏好并通过 OutputState 报告。
